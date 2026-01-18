use crate::protocol::{Packet, PacketCodec};
use crate::utils::get_socket_dir;
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::isatty;
use std::io::{self};
use std::os::fd::{AsRawFd, BorrowedFd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::codec::Framed;

pub struct RawModeGuard {
    fd: std::os::fd::RawFd,
    original: Termios,
}

impl RawModeGuard {
    pub fn new(fd: std::os::fd::RawFd) -> Result<Self> {
        // SAFETY: The BorrowedFd does not own the file descriptor.
        // The caller guarantees that `fd` remains valid for the lifetime
        // of this guard. Since stdin is never closed during the program's
        // execution, this is safe.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(borrowed, SetArg::TCSANOW, &raw)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: Same as new() - fd is guaranteed valid by the caller.
        // The guard owns the original termios state and restores it on drop.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

fn get_terminal_size() -> Result<(u16, u16)> {
    let winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ is a standard ioctl call that reads terminal window
    // size into a valid winsize struct. STDOUT_FILENO is always a valid FD.
    // The ioctl does not take ownership of any resources.
    unsafe {
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &winsize) != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok((winsize.ws_col, winsize.ws_row))
}

async fn process_stdin(stdin: &mut tokio::io::Stdin, mut stdin_buf: &mut [u8]) -> Option<Packet> {
    match stdin.read(&mut stdin_buf).await {
        Ok(0) => None,
        Ok(n) => {
            let data = bytes::Bytes::copy_from_slice(&stdin_buf[..n]);
            Some(Packet::Data(data))
        }
        Err(_) => None,
    }
}

async fn process_signals(sigwinch: &mut tokio::signal::unix::Signal) -> Option<Packet> {
    sigwinch.recv().await?;
    if let Ok((cols, rows)) = get_terminal_size() {
        Some(Packet::Resize(cols, rows))
    } else {
        None
    }
}

pub async fn attach(session_name: String) -> Result<()> {
    let socket_path = get_socket_dir()?.join(format!("{}.sock", session_name));

    // Retry logic since daemon might be starting up
    let mut attempts = 0;
    let stream = loop {
        match UnixStream::connect(&socket_path).await {
            Ok(s) => break s,
            Err(e) => {
                if attempts > 10 {
                    return Err(e.into());
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                attempts += 1;
            }
        }
    };

    let framed = Framed::new(stream, PacketCodec);

    let stdin_fd = std::io::stdin().as_raw_fd();
    if !isatty(stdin_fd)? {
        anyhow::bail!("stdin is not a TTY");
    }

    // Enable Raw Mode
    let _guard = RawModeGuard::new(stdin_fd)?;

    // Split stream
    let (mut sink, mut stream) = framed.split();

    // 1. Sink Writer Task (Channel -> Socket)
    let mut stdin = tokio::io::stdin();
    let writer_handle = tokio::spawn(async move {
        if let Ok((cols, rows)) = get_terminal_size() {
            let _ = sink.send(Packet::Resize(cols, rows)).await;
        }
        let mut sigwinch = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut stdin_buf = [0u8; 4096];

        loop {
            let packet = tokio::select! {
                res = process_stdin(&mut stdin, &mut stdin_buf) => res,
                res = process_signals(&mut sigwinch) => res,
            };
            if let Some(packet) = packet
                && !sink.send(packet).await.is_err()
            {
                continue;
            }
            break;
        }
    });

    // 5. Socket -> Stdout (Blocking Main Task)
    let mut stdout = tokio::io::stdout();
    while let Some(result) = stream.next().await {
        match result {
            Ok(Packet::Data(data)) => {
                if stdout.write_all(&data).await.is_err() {
                    break;
                }
                if stdout.flush().await.is_err() {
                    break;
                }
            }
            Ok(Packet::Exit(status)) => {
                // Client handle exit message
                let msg = status.format_message();
                let _ = stdout.write_all(msg.as_bytes()).await;
                let _ = stdout.flush().await;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // Cleanup
    writer_handle.abort();

    Ok(())
}
