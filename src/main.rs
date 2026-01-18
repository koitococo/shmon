mod client;
mod protocol;
mod server;
mod utils;

use crate::utils::get_socket_dir;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::process::exit;

#[derive(Parser)]
#[command(name = "shmon")]
#[command(about = "A minimal terminal multiplexer", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Session name (for start/attach)
    #[arg(index = 1)]
    session: Option<String>,

    /// Shell command to run (e.g. "/bin/bash", "vim"). Defaults to $SHELL.
    #[arg(short, long)]
    shell: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// List all running sessions
    List,
    /// Kill a session
    Kill { session: String },
}

fn main() {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::List) => list_sessions(),
        Some(Commands::Kill { session }) => kill_session(session),
        None => {
            if let Some(session) = cli.session {
                let shell = cli.shell.unwrap_or_else(|| {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
                });
                start_or_attach(session, shell)
            } else {
                eprintln!("Usage: shmon <session> OR shmon <command>");
                exit(1);
            }
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        exit(1);
    }
}

fn start_or_attach(session: String, shell_cmd: String) -> Result<()> {
    let socket_dir = get_socket_dir()?;
    let socket_path = socket_dir.join(format!("{}.sock", session));
    let pid_path = socket_dir.join(format!("{}.pid", session));

    // Check if session is actually running
    let mut is_running = false;
    if pid_path.exists()
        && let Ok(pid_str) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = pid_str.trim().parse::<i32>()
        && check_process(pid)
    {
        is_running = true;
    }

    // Clean up stale socket if not running
    if !is_running && socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    if !is_running {
        println!(
            "Starting new session: {} with shell: {}",
            session, shell_cmd
        );
        server::spawn_daemon(session.clone(), shell_cmd)?;

        // Wait briefly for the daemon to create the socket
        for _ in 0..20 {
            if socket_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    } else {
        println!("Attaching to session: {}", session);
    }

    // Attach
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(client::attach(session))
}

fn list_sessions() -> Result<()> {
    let socket_dir = get_socket_dir()?;

    if !socket_dir.exists() {
        println!("No sessions found.");
        return Ok(());
    }

    println!("Active sessions:");
    let mut found = false;
    for entry in std::fs::read_dir(socket_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension()
            && ext == "pid"
            && let Some(stem) = path.file_stem()
        {
            let session_name = stem.to_string_lossy();
            // Check PID
            if let Ok(pid_str) = std::fs::read_to_string(&path)
                && let Ok(pid) = pid_str.trim().parse::<i32>()
                && check_process(pid)
            {
                println!("  {} (PID {})", session_name, pid);
                found = true;
            }
        }
    }
    if !found {
        println!("  (none)");
    }
    Ok(())
}

fn kill_session(session: String) -> Result<()> {
    let socket_dir = get_socket_dir()?;
    let pid_path = socket_dir.join(format!("{}.pid", session));

    if !pid_path.exists() {
        anyhow::bail!("Session '{}' not found (no pid file).", session);
    }

    let pid_str = std::fs::read_to_string(&pid_path)?;
    let pid: i32 = pid_str.trim().parse()?;

    // Send SIGTERM
    let pgid = nix::unistd::Pid::from_raw(pid);
    nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGTERM)?;

    // Wait for process to exit (up to 2 seconds)
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if nix::sys::signal::kill(pgid, None).is_err() {
            break;
        }
    }

    // Clean up files
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(socket_dir.join(format!("{}.sock", session)));

    println!("Killed session: {}", session);
    Ok(())
}

fn check_process(pid: i32) -> bool {
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
        return false;
    }
    if let Ok(current_exe) = std::env::current_exe() {
        let proc_exe = std::path::PathBuf::from(format!("/proc/{}/exe", pid));
        if let Ok(target_exe) = std::fs::read_link(proc_exe) {
            if current_exe == target_exe {
                return true;
            }
            if let (Some(n1), Some(n2)) = (current_exe.file_name(), target_exe.file_name())
                && n1 == n2
            {
                return true;
            }
        }
    }
    false
}
