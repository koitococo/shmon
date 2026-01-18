pub mod backlog;
pub mod daemon;
pub mod pty;

use crate::utils::get_socket_dir;
use anyhow::Result;
use nix::unistd::{ForkResult, fork, setsid};
use std::fs::File;
use std::process::exit;

/// Spawns a daemon for the given session.
pub fn spawn_daemon(session_name: String, shell_cmd: String) -> Result<()> {
    match unsafe { fork()? } {
        ForkResult::Parent { .. } => Ok(()),
        ForkResult::Child => {
            setsid()?;
            match unsafe { fork()? } {
                ForkResult::Parent { .. } => {
                    exit(0);
                }
                ForkResult::Child => {
                    // Daemon Logic
                    let socket_dir = match get_socket_dir() {
                        Ok(d) => d,
                        Err(_) => exit(1),
                    };
                    let log_path = socket_dir.join(format!("{}.log", session_name));

                    if let Ok(log_file) = File::create(&log_path) {
                        let fd = std::os::fd::AsRawFd::as_raw_fd(&log_file);
                        let _ = nix::unistd::dup2(fd, 1);
                        let _ = nix::unistd::dup2(fd, 2);
                    } else {
                        let _ = nix::unistd::close(1);
                        let _ = nix::unistd::close(2);
                    }
                    let _ = nix::unistd::close(0);

                    // 1. Spawn PTY Shell (Safe Fork before Runtime)
                    let pty_handle = match pty::spawn_pty_shell(shell_cmd) {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("Failed to spawn PTY: {}", e);
                            exit(1);
                        }
                    };

                    // Start Runtime
                    let rt = match tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            eprintln!("Failed to start runtime: {}", e);
                            exit(1);
                        }
                    };

                    if let Err(e) = rt.block_on(daemon::run(session_name, pty_handle)) {
                        eprintln!("Daemon error: {}", e);
                        exit(1);
                    }
                    exit(0);
                }
            }
        }
    }
}
