use crate::protocol::{Packet, PacketCodec};
use crate::server::backlog::Backlog;
use crate::server::pty::{PtyHandle, read_from_master, resize_pty, signal_child, write_to_master};
use crate::utils::get_socket_dir;
use anyhow::Result;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::sys::wait::{WaitPidFlag, waitpid};
use std::fs::Permissions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio_util::codec::Framed;
use tracing::{debug, error, info, warn};

/// Maximum number of concurrent client connections.
const MAX_CLIENTS: usize = 8;

pub async fn run(session_name: String, pty_handle: PtyHandle) -> Result<()> {
    // 1. Setup paths
    let socket_dir = get_socket_dir()?;
    let socket_path = socket_dir.join(format!("{}.sock", session_name));
    let pid_path = socket_dir.join(format!("{}.pid", session_name));

    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    // Write PID file
    let pid = std::process::id();
    std::fs::write(&pid_path, pid.to_string())?;

    // Bind Socket
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, Permissions::from_mode(0o600))?;

    info!(session = %session_name, path = ?socket_path, "Daemon started");

    // 2. Wrap PTY in AsyncFd
    let master_async = AsyncFd::new(pty_handle.master)?;
    let master = Arc::new(master_async);
    let child_pid = pty_handle.child_pid;

    // 3. Setup channels
    let (broadcast_tx, _) = broadcast::channel::<Packet>(100);
    let backlog = Arc::new(Mutex::new(Backlog::new()));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Packet>(32);

    // 4. Signal Handlers
    let mut sig_term = signal(SignalKind::terminate())?;
    let mut sig_int = signal(SignalKind::interrupt())?;

    // 5. Spawn PTY reader as independent task
    // This resolves the select! starvation issue (C1): PTY reading no longer
    // blocks the main loop from accepting clients or processing commands.
    let pty_broadcast = broadcast_tx.clone();
    let pty_backlog = backlog.clone();
    let pty_master = master.clone();
    let pty_child_pid = child_pid;
    let mut pty_join_set = JoinSet::new();

    pty_join_set.spawn(async move {
        loop {
            match read_from_master(&pty_master).await {
                Ok(data) => {
                    if data.is_empty() {
                        info!("PTY closed (EOF)");
                        break;
                    }
                    let bytes = Bytes::from(data);
                    {
                        let mut bl = pty_backlog.lock().await;
                        bl.push(bytes.clone());
                    }
                    let _ = pty_broadcast.send(Packet::Data(bytes));
                }
                Err(e) => {
                    error!("PTY read error: {}", e);
                    break;
                }
            }
        }
        let exit_status = match waitpid(pty_child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::Exited(_pid, code)) => {
                crate::protocol::ExitStatus::from_code(code)
            }
            Ok(nix::sys::wait::WaitStatus::Signaled(_pid, sig, _core)) => {
                crate::protocol::ExitStatus::from_signal(sig as i32)
            }
            _ => {
                crate::protocol::ExitStatus { code: None, signal: None }
            }
        };
        let _ = pty_broadcast.send(Packet::Exit(exit_status));
        // Wait for broadcast delivery to connected clients
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    });

    // 6. Client management via JoinSet
    let mut client_set = JoinSet::new();

    // 7. Main Loop — no longer blocked by PTY reads
    loop {
        tokio::select! {
            // Shutdown Signals
            _ = sig_term.recv() => {
                info!("Received SIGTERM, shutting down");
                break;
            }
            _ = sig_int.recv() => {
                info!("Received SIGINT, shutting down");
                break;
            }

            // Accept Clients
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        if client_set.len() >= MAX_CLIENTS {
                            warn!(max_clients = MAX_CLIENTS, "Max clients reached, rejecting connection");
                            continue;
                        }
                        debug!(clients = client_set.len() + 1, "New client connected");
                        let broadcast_rx = broadcast_tx.subscribe();
                        let cmd_tx_clone = cmd_tx.clone();
                        let backlog_clone = backlog.clone();

                        client_set.spawn(async move {
                            if let Err(e) = handle_client(stream, broadcast_rx, cmd_tx_clone, backlog_clone).await {
                                // Distinguish BrokenPipe (normal disconnect) from real errors
                                if e.root_cause().downcast_ref::<std::io::Error>()
                                    .map(|io_err| io_err.kind() != std::io::ErrorKind::BrokenPipe)
                                    .unwrap_or(true)
                                {
                                    error!("Client handler error: {}", e);
                                }
                            }
                        });
                    }
                    Err(e) => error!("Accept error: {}", e),
                }
            }

            // Handle Commands from clients
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    Packet::Data(data) => {
                        if let Err(e) = write_to_master(&master, &data).await {
                            error!("PTY write error: {}", e);
                        }
                    }
                    Packet::Resize(cols, rows) => {
                        let raw_fd = master.get_ref().as_raw_fd();
                        if let Err(e) = resize_pty(raw_fd, cols, rows) {
                            warn!("Resize error: {}", e);
                        }
                    }
                    Packet::Signal(sig) => {
                        if let Err(e) = signal_child(child_pid, sig) {
                            warn!("Signal child error: {}", e);
                        }
                    }
                    Packet::Exit(_) => {
                        // Exit is sent from daemon to client, not the other way around
                        warn!("Received unexpected Exit packet from client");
                    }
                }
            }

            // Monitor completed client tasks
            Some(result) = client_set.join_next() => {
                if let Err(e) = result
                    && !e.is_cancelled()
                {
                    error!("Client task panicked: {}", e);
                }
            }

            // Monitor PTY task completion (shell exit)
            Some(result) = pty_join_set.join_next() => {
                if let Err(e) = result
                    && !e.is_cancelled()
                {
                    error!("PTY task failed: {}", e);
                }
                info!("PTY task ended, shutting down");
                break;
            }
        }
    }

    // 8. Graceful Shutdown
    info!("Shutting down daemon...");

    // Brief pause to let pending broadcast messages (e.g. exit status)
    // reach connected clients before we tear everything down.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Abort PTY reader
    pty_join_set.abort_all();

    // Abort all client tasks
    client_set.abort_all();

    // Brief pause to let aborts propagate
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Cleanup filesystem artifacts
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    let _ = signal_child(child_pid, libc::SIGKILL);

    info!("Daemon shutdown complete");
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    mut broadcast_rx: broadcast::Receiver<Packet>,
    cmd_tx: mpsc::Sender<Packet>,
    backlog: Arc<Mutex<Backlog>>,
) -> Result<()> {
    let mut framed = Framed::new(stream, PacketCodec);

    // Send backlog history to newly connected client
    {
        let bl = backlog.lock().await;
        for chunk in bl.iter() {
            framed.send(Packet::Data(chunk.clone())).await?;
        }
    }

    let (mut sink, mut stream) = framed.split();
    let cmd_tx_clone = cmd_tx.clone();

    // Reader: client → daemon commands
    let mut reader_task = tokio::spawn(async move {
        while let Some(result) = stream.next().await {
            match result {
                Ok(packet) => {
                    if cmd_tx_clone.send(packet).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Writer: daemon PTY output → client
    // Handles broadcast::RecvError::Lagged gracefully (I2)
    let mut writer_task = tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(Packet::Data(bytes)) => {
                    if sink.send(Packet::Data(bytes)).await.is_err() {
                        break;
                    }
                }
                Ok(Packet::Exit(e)) => {
                    let _ = sink.send(Packet::Exit(e)).await;
                    break;
                }
                Ok(_) => {
                    // Ignore other packet types (Resize, Signal)
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "Client lagged behind, skipping messages");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Wait for either reader or writer to finish
    tokio::select! {
        _ = &mut reader_task => {},
        _ = &mut writer_task => {},
    }

    // Clean up the other task
    reader_task.abort();
    writer_task.abort();
    debug!("Client disconnected");
    Ok(())
}
