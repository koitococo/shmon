use anyhow::Result;
use nix::pty::openpty;
use nix::unistd::{ForkResult, dup2, execvp, fork, setsid};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{env, io};
use tokio::io::unix::AsyncFd;

pub struct PtyHandle {
    pub master: std::fs::File,
    pub child_pid: nix::unistd::Pid,
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL)?;
    let mut flags = nix::fcntl::OFlag::from_bits_truncate(flags);
    flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
    nix::fcntl::fcntl(fd, nix::fcntl::F_SETFL(flags))?;
    Ok(())
}

pub fn spawn_pty_shell(shell_cmd: String) -> Result<PtyHandle> {
    let openpty_result = openpty(None, None)?;
    match unsafe { fork()? } {
        ForkResult::Child => {
            // Child process
            setsid()?;

            let slave = openpty_result.slave;
            drop(openpty_result.master);

            let slave_fd = slave.as_raw_fd();

            // Set Controlling Terminal
            // This is critical for job control
            unsafe {
                if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) != 0 {
                    // We can't use standard eprintln here safely if fd 2 is closed/redirected?
                    // But we haven't dup2 yet.
                    // Ignore error? If we fail here, job control won't work.
                }
            }

            dup2(slave_fd, libc::STDIN_FILENO)?;
            dup2(slave_fd, libc::STDOUT_FILENO)?;
            dup2(slave_fd, libc::STDERR_FILENO)?;

            // SAFETY: `env::set_var` is unsafe in Rust 2024 edition because it can race
            // with other threads accessing the environment. This is safe here because
            // we are in a forked child process with a single thread, so no concurrent
            // access is possible.
            if env::var("TERM").is_err() {
                unsafe {
                    env::set_var("TERM", "xterm-256color");
                }
            }

            // Split shell_cmd into executable and args
            // Simple splitting by space. For complex cases users should use proper quoting,
            // but shmon is simple. Or we just execute the shell directly if it's a path.
            // If user passes "/bin/bash", args is ["/bin/bash"].
            // If user passes "vim", args is ["vim"].
            // Requirement says "allow user to modify what shell to start".

            let parts: Vec<&str> = shell_cmd.split_whitespace().collect();
            let cmd = if parts.is_empty() {
                env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
            } else {
                parts[0].to_string()
            };

            let cmd_cstr = CString::new(cmd.clone())?;

            // Args must start with the program name
            let mut args_cstrs = Vec::new();
            if parts.is_empty() {
                args_cstrs.push(cmd_cstr.clone());
            } else {
                for part in parts {
                    args_cstrs.push(CString::new(part)?);
                }
            }

            // execvp expects &[&CStr]
            let args_ref: Vec<&std::ffi::CStr> = args_cstrs.iter().map(|c| c.as_c_str()).collect();

            execvp(&cmd_cstr, &args_ref)?;

            std::process::exit(1);
        }
        ForkResult::Parent { child } => {
            // Parent process
            drop(openpty_result.slave);

            let master = openpty_result.master;
            let master_fd = master.into_raw_fd();

            // Set non-blocking
            set_nonblocking(master_fd)?;

            let master_file = unsafe { std::fs::File::from_raw_fd(master_fd) };

            Ok(PtyHandle {
                master: master_file,
                child_pid: child,
            })
        }
    }
}

/// Reads data from the PTY master file descriptor.
///
/// Allocates a new 8KB buffer per call. For high-throughput scenarios
/// (e.g., `cat` of large files), consider using a buffer pool or `BytesMut`
/// reuse. Current implementation is acceptable for terminal multiplexer
/// workloads where data rates are moderate.
pub async fn read_from_master(master: &AsyncFd<std::fs::File>) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; 8192];
    loop {
        let mut guard = master.readable().await?;
        let result = guard.try_io(|inner| {
            let mut file = inner.get_ref();
            let n = file.read(&mut buffer)?;
            Ok(n)
        });
        match result {
            Ok(Ok(bytes)) => {
                buffer.truncate(bytes);
                return Ok(buffer);
            }
            Ok(Err(err)) => {
                if err.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(err);
            }
            Err(_would_block) => continue,
        }
    }
}

pub async fn write_to_master(master: &AsyncFd<std::fs::File>, data: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let mut guard = master.writable().await?;
        let result = guard.try_io(|inner| {
            let mut file = inner.get_ref();
            let written = file.write(&data[offset..])?;
            Ok(written)
        });
        match result {
            Ok(Ok(written)) => offset += written,
            Ok(Err(err)) => {
                if err.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(err);
            }
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

pub fn resize_pty(master_fd: RawFd, cols: u16, rows: u16) -> Result<()> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let res = unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &winsize) };
    if res != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn signal_child(pid: nix::unistd::Pid, signal: i32) -> Result<()> {
    let sig = nix::sys::signal::Signal::try_from(signal)?;
    nix::sys::signal::kill(pid, sig)?;
    Ok(())
}
