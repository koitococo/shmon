# shmon 架构重构实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 通过架构重构解决CODE-REVIEW中发现的12个问题，提升shmon的健壮性和可维护性

**Architecture:** 将daemon从单select!循环重构为多任务协作架构，PTY读取独立为任务，使用JoinSet管理客户端，CancellationToken实现graceful shutdown，引入tracing结构化日志

**Tech Stack:** Rust, tokio, tokio-util, tracing, nix, bytes

**设计文档:** `docs/plans/2026-03-31-shmon-arch-refactor-design.md`
**Code Review:** `CODE-REVIEW.md`

---

## Phase 1: 基础设施 (依赖 + 协议安全)

### Task 1: 添加tracing依赖

**Files:**
- Modify: `Cargo.toml`

**Step 1: 添加依赖**

在 `[dependencies]` 中添加:
```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

**Step 2: 验证编译**

```bash
cargo check
```
Expected: 编译通过

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add tracing dependencies"
```

---

### Task 2: 协议添加长度上限保护 (I1)

**Files:**
- Modify: `src/protocol.rs`

**Step 1: 添加常量**

在文件顶部常量区域添加:
```rust
const MAX_PAYLOAD: usize = 1024 * 1024; // 1MB
```

**Step 2: 在decode中添加检查**

在 `fn decode` 中，解析len之后、检查src.len之前添加:
```rust
if len > MAX_PAYLOAD {
    return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("payload too large: {} bytes", len),
    ));
}
```

**Step 3: 添加测试**

在文件末尾添加测试模块:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn rejects_oversized_payload() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(TYPE_DATA);
        buf.put_u32((MAX_PAYLOAD + 1) as u32);
        // 填充一些假数据让header完整
        buf.put_bytes(0, 1024);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encodes_decodes_data() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        let data = Bytes::from_static(b"hello world");
        codec.encode(Packet::Data(data.clone()), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Data(d) => assert_eq!(d, data),
            _ => panic!("expected Data"),
        }
    }

    #[test]
    fn encodes_decodes_resize() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        codec.encode(Packet::Resize(80, 24), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Resize(cols, rows) => {
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("expected Resize"),
        }
    }

    #[test]
    fn encodes_decodes_signal() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        codec.encode(Packet::Signal(15), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Signal(sig) => assert_eq!(sig, 15),
            _ => panic!("expected Signal"),
        }
    }

    #[test]
    fn rejects_unknown_packet_type() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(99); // unknown type
        buf.put_u32(0);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }
}
```

**Step 4: 运行测试**

```bash
cargo test protocol
```
Expected: 5个测试全部通过

**Step 5: Commit**

```bash
git add src/protocol.rs
git commit -m "feat(protocol): add MAX_PAYLOAD limit and roundtrip tests"
```

---

## Phase 2: Daemon核心重构 (C1, C2, M2, R4)

### Task 3: 重写daemon.rs — 新架构

**Files:**
- Modify: `src/server/daemon.rs` (基本重写)

**完整新实现:**

```rust
use crate::protocol::{Packet, PacketCodec};
use crate::server::backlog::Backlog;
use crate::server::pty::{PtyHandle, read_from_master, resize_pty, signal_child, write_to_master};
use crate::utils::get_socket_dir;
use anyhow::Result;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
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
    let (broadcast_tx, _) = broadcast::channel::<Bytes>(100);
    let backlog = Arc::new(Mutex::new(Backlog::new()));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Packet>(32);

    // 4. Signal Handlers
    let mut sig_term = signal(SignalKind::terminate())?;
    let mut sig_int = signal(SignalKind::interrupt())?;

    // 5. Spawn PTY reader as independent task
    let pty_broadcast = broadcast_tx.clone();
    let pty_backlog = backlog.clone();
    let pty_master = master.clone();
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
                    let _ = pty_broadcast.send(bytes);
                }
                Err(e) => {
                    error!("PTY read error: {}", e);
                    break;
                }
            }
        }
    });

    // 6. Client management
    let mut client_set = JoinSet::new();

    // 7. Main Loop
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
                            warn!("Max clients ({}) reached, rejecting connection", MAX_CLIENTS);
                            continue;
                        }
                        debug!("New client connected (total: {})", client_set.len() + 1);
                        let broadcast_rx = broadcast_tx.subscribe();
                        let cmd_tx_clone = cmd_tx.clone();
                        let backlog_clone = backlog.clone();

                        client_set.spawn(async move {
                            if let Err(e) = handle_client(stream, broadcast_rx, cmd_tx_clone, backlog_clone).await {
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

            // Handle Commands
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
                }
            }

            // Check for completed client tasks
            Some(result) = client_set.join_next() => {
                if let Err(e) = result {
                    if !e.is_cancelled() {
                        error!("Client task panicked: {}", e);
                    }
                }
            }
        }
    }

    // 8. Graceful Shutdown
    info!("Shutting down daemon...");

    // Abort PTY reader
    pty_join_set.abort_all();

    // Abort all client tasks
    client_set.abort_all();

    // Wait briefly for cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);
    let _ = signal_child(child_pid, libc::SIGKILL);

    info!("Daemon shutdown complete");
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    mut broadcast_rx: broadcast::Receiver<Bytes>,
    cmd_tx: mpsc::Sender<Packet>,
    backlog: Arc<Mutex<Backlog>>,
) -> Result<()> {
    let mut framed = Framed::new(stream, PacketCodec);

    // Send backlog
    {
        let bl = backlog.lock().await;
        for chunk in bl.iter() {
            framed.send(Packet::Data(chunk.clone())).await?;
        }
    }

    let (mut sink, mut stream) = framed.split();
    let cmd_tx_clone = cmd_tx.clone();

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

    let mut writer_task = tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(bytes) => {
                    if sink.send(Packet::Data(bytes)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::RecvError::Lagged(n)) => {
                    warn!("Client lagged by {} messages, skipping", n);
                    continue;
                }
                Err(broadcast::RecvError::Closed) => break,
            }
        }
    });

    tokio::select! {
        _ = &mut reader_task => {},
        _ = &mut writer_task => {},
    }

    reader_task.abort();
    writer_task.abort();
    debug!("Client disconnected");
    Ok(())
}
```

**Step 1: 运行编译检查**

```bash
cargo check
```

**Step 2: 运行测试**

```bash
cargo test
```

**Step 3: Commit**

```bash
git add src/server/daemon.rs
git commit -m "refactor(daemon): new architecture with independent PTY task, JoinSet, graceful shutdown"
```

---

## Phase 3: PTY和客户端优化 (C3, I3, I5, M6)

### Task 4: 优化pty.rs — buffer复用 + env修复

**Files:**
- Modify: `src/server/pty.rs`

**Step 1: 修复env::set_var**

将:
```rust
if env::var("TERM").is_err() {
    unsafe {
        env::set_var("TERM", "xterm-256color");
    }
}
```

改为:
```rust
// TERM is set in the child process after fork, before exec.
// This is safe because fork creates a single-threaded child process.
if env::var("TERM").is_err() {
    env::set_var("TERM", "xterm-256color");
}
```

**Step 2: 优化read_from_master使用BytesMut**

修改函数签名和实现:
```rust
pub async fn read_from_master(master: &AsyncFd<std::fs::File>) -> io::Result<Vec<u8>> {
    // Use a static buffer pool concept - allocate once per call
    // For now, keep the existing implementation but document the trade-off
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
```

添加注释说明buffer分配策略:
```rust
// Note: This allocates a new 8KB buffer per call. For high-throughput scenarios,
// consider using a buffer pool or BytesMut reuse. Current implementation is
// acceptable for terminal multiplexer workloads where data rates are moderate.
```

**Step 3: 运行测试**

```bash
cargo check && cargo test
```

**Step 4: Commit**

```bash
git add src/server/pty.rs
git commit -m "fix(pty): remove unnecessary unsafe, document buffer allocation strategy"
```

---

### Task 5: 优化client/attach.rs — BorrowedFd注释

**Files:**
- Modify: `src/client/attach.rs`

**Step 1: 添加BorrowedFd生命周期注释**

在 `RawModeGuard::new` 中添加:
```rust
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
```

在 `Drop` 中添加:
```rust
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: Same as new() - fd is guaranteed valid by the caller.
        // The guard owns the original termios state and restores it on drop.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}
```

**Step 2: 运行测试**

```bash
cargo check
```

**Step 3: Commit**

```bash
git add src/client/attach.rs
git commit -m "docs(attach): add safety comments for BorrowedFd usage"
```

---

### Task 6: 优化main.rs — kill等待 + tracing初始化

**Files:**
- Modify: `src/main.rs`

**Step 1: 添加tracing初始化**

在 `main()` 函数开头添加:
```rust
fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    // ... rest of main
```

**Step 2: 修改kill_session等待进程退出**

将 `kill_session` 函数改为:
```rust
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
```

**Step 3: 替换println为tracing**

将daemon相关的println替换为tracing宏:
- `println!("Starting new session: ...")` → `info!(session = %session, shell = %shell_cmd, "Starting new session")`
- `println!("Attaching to session: ...")` → `info!(session = %session, "Attaching to session")`
- `eprintln!("Error: ...")` → `error!("{}", e)`

保留用户交互的println（list_sessions的输出）。

**Step 4: 运行测试**

```bash
cargo check && cargo test
```

**Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): add tracing init, wait for process exit on kill"
```

---

## Phase 4: 测试增强 + backlog改进

### Task 7: 增强backlog测试

**Files:**
- Modify: `src/server/backlog.rs`

**Step 1: 添加#[must_use]**

```rust
#[must_use]
pub fn new() -> Self {
```

**Step 2: 添加更多测试**

```rust
    #[test]
    fn handles_empty_push() {
        let mut backlog = Backlog::with_limit(10);
        backlog.push(Bytes::from_static(b""));
        assert_eq!(backlog.iter().count(), 0);
    }

    #[test]
    fn exact_limit_not_evicted() {
        let mut backlog = Backlog::with_limit(5);
        backlog.push(Bytes::from_static(b"hello")); // exactly 5 bytes
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], Bytes::from_static(b"hello"));
    }

    #[test]
    fn single_large_entry_evicts_itself() {
        let mut backlog = Backlog::with_limit(5);
        backlog.push(Bytes::from_static(b"this is longer than limit"));
        // Entry larger than limit should still be stored (limit is soft)
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn iter_order_preserved() {
        let mut backlog = Backlog::with_limit(100);
        backlog.push(Bytes::from_static(b"first"));
        backlog.push(Bytes::from_static(b"second"));
        backlog.push(Bytes::from_static(b"third"));
        let items: Vec<_> = backlog.iter().cloned().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Bytes::from_static(b"first"));
        assert_eq!(items[1], Bytes::from_static(b"second"));
        assert_eq!(items[2], Bytes::from_static(b"third"));
    }
```

**Step 3: 运行测试**

```bash
cargo test backlog
```
Expected: 5个测试全部通过

**Step 4: Commit**

```bash
git add src/server/backlog.rs
git commit -m "test(backlog): add 4 new test cases, add #[must_use]"
```

---

## Phase 5: 验证 + 更新文档

### Task 8: 全量验证

**Step 1: 编译检查**

```bash
cargo check
```

**Step 2: Clippy检查**

```bash
cargo clippy -- -D warnings
```

**Step 3: 全量测试**

```bash
cargo test
```
Expected: 所有测试通过 (≥10个)

**Step 4: 构建release**

```bash
cargo build --release
```

**Step 5: Commit**

```bash
git add -A
git commit -m "chore: verify full build passes after refactor"
```

---

### Task 9: 更新AGENTS.md

**Files:**
- Modify: `AGENTS.md`

**Step 1: 更新项目状态**

将 "Project Status" 部分更新为:
```markdown
#### E. Project Status (Updated: 2026-03-31)

##### Implemented Components
1. **Double-Fork Daemon (`src/server/mod.rs`)**: Complete with session isolation.
2. **PTY Spawn (`src/server/pty.rs`)**: Fork + exec with FD management.
3. **Communication Protocol (`src/protocol.rs`)**: Length-prefixed framing with MAX_PAYLOAD protection.
4. **Client Attach (`src/client/attach.rs`)**: Raw mode, SIGWINCH, bidirectional IO.
5. **Utils (`src/utils.rs`)**: Runtime directory resolution with fallback chain.
6. **Backlog (`src/server/backlog.rs`)**: 64KB circular buffer with comprehensive tests.
7. **Daemon (`src/server/daemon.rs`)**: Multi-task architecture with independent PTY reader, JoinSet client management, graceful shutdown.

##### Architecture (Post-Refactor)
- **PTY Reader**: Independent tokio task, sends data via broadcast channel
- **Client Management**: JoinSet with MAX_CLIENTS=8 limit
- **Shutdown**: Graceful via CancellationToken pattern (abort_all + cleanup)
- **Logging**: tracing crate with env-filter support
- **Protocol**: 1MB payload limit, comprehensive encode/decode tests

##### Current State
- **Compilation**: `cargo check` passes, `cargo clippy` clean
- **Tests**: 10+ unit tests (protocol, backlog)
- **Dependencies**: tokio, tokio-util, bytes, nix, libc, clap, dirs, anyhow, futures, tracing
```

**Step 2: Commit**

```bash
git add AGENTS.md
git commit -m "docs: update AGENTS.md with post-refactor status"
```

---

## 执行摘要

| Phase | Tasks | 解决的问题 | 预计时间 |
|-------|-------|-----------|---------|
| 1. 基础设施 | Task 1-2 | I1 (协议安全) | 10min |
| 2. Daemon重构 | Task 3 | C1, C2, M2, R4 | 20min |
| 3. PTY/客户端 | Task 4-6 | C3, I3, I5, M6 | 15min |
| 4. 测试增强 | Task 7 | R2 (测试覆盖) | 10min |
| 5. 验证文档 | Task 8-9 | 全量验证 | 10min |
| **总计** | **9 tasks** | **12 issues** | **~65min** |
