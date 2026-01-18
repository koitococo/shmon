# shmon 架构重构设计文档

> 日期: 2026-03-31
> 状态: 待审批
> 目标: 通过架构重构解决CODE-REVIEW中发现的Critical和Important问题

---

## 1. 重构目标

### 解决的问题
| 编号 | 问题 | 解决方式 |
|------|------|----------|
| C1 | PTY读取饥饿accept/command | PTY读取拆为独立任务 |
| C2 | handle_client abort不完整 | CancellationToken优雅退出 |
| C3 | 每次PTY读取分配buffer | buffer复用/BytesMut |
| I1 | 协议缺少长度上限 | 添加MAX_PAYLOAD检查 |
| I2 | broadcast慢消费者 | 区分Lagged/Closed错误 |
| I3 | fork后env::set_var unsafe | 改用Command::env() |
| I5 | BorrowedFd生命周期 | 添加注释/改用OwnedFd |
| M2 | 无客户端数量限制 | 添加MAX_CLIENTS |
| M6 | kill不等待进程退出 | 添加等待逻辑 |
| R1 | 无结构化日志 | 引入tracing crate |
| R4 | 无graceful shutdown | JoinSet + CancellationToken |
| R5 | PTY逻辑复杂 | 考虑forkpty简化 |

---

## 2. 架构设计

### 2.1 新Daemon事件循环架构

**当前架构** (单select!循环):
```
select! {
    signal → break
    accept → spawn handler
    pty_read → process
    command → execute
}
```

**新架构** (多任务协作):
```
┌─────────────────────────────────────────────────┐
│                    Daemon Main                   │
│  ┌─────────────┐  ┌──────────────┐              │
│  │ PTY Reader  │  │ Command Rx   │              │
│  │ (独立任务)   │  │ (主循环)      │              │
│  │             │  │              │              │
│  │ read → ch   │  │ accept →     │              │
│  │             │  │   spawn      │              │
│  └──────┬──────┘  └──────┬───────┘              │
│         │ broadcast       │                      │
│         ▼                 ▼                      │
│  ┌─────────────┐  ┌──────────────┐              │
│  │ Broadcast   │  │ Client       │              │
│  │ Channel     │  │ Handlers     │              │
│  │ (PTY→Client)│  │ (JoinSet)    │              │
│  └─────────────┘  └──────────────┘              │
└─────────────────────────────────────────────────┘
```

**关键变化**:
- PTY读取不再阻塞select!，改为独立任务通过channel发送数据
- 主循环只处理: 信号、accept、command接收
- 使用 `JoinSet` 管理所有spawn的任务
- 使用 `CancellationToken` 实现graceful shutdown

### 2.2 客户端管理

**当前**: 每个客户端独立spawn，无上限，无统一管理
**新设计**:
```rust
struct ClientManager {
    clients: JoinSet<()>,
    token: CancellationToken,
    max_clients: usize,  // 默认8
}
```

- 新连接时检查 `clients.len() < max_clients`
- 每个客户端handler接收CancellationToken
- shutdown时cancel所有客户端，await JoinSet完成

### 2.3 协议安全

```rust
const MAX_PAYLOAD: usize = 1024 * 1024; // 1MB

// Decoder中:
if len > MAX_PAYLOAD {
    return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("payload too large: {} bytes", len),
    ));
}
```

### 2.4 Broadcast慢消费者处理

```rust
// handle_client writer中:
loop {
    match broadcast_rx.recv().await {
        Ok(bytes) => { /* send */ }
        Err(RecvError::Lagged(n)) => {
            tracing::warn!("client lagged by {} messages, skipping", n);
            continue;
        }
        Err(RecvError::Closed) => break,
    }
}
```

### 2.5 PTY简化 (可选)

评估 `nix::pty::forkpty` 替代当前 `openpty + fork + dup2 + setsid + ioctl` 的可行性。

**当前流程** (约80行):
```
openpty → fork → child: setsid → ioctl(TIOCSCTTY) → dup2×3 → execvp
                    parent: set_nonblocking → from_raw_fd
```

**forkpty流程** (约30行):
```
forkpty → child: setsid → execvp
          parent: set_nonblocking → from_raw_fd
```

**风险**: forkpty在某些平台可能行为不一致，需要验证Linux兼容性。

---

## 3. 依赖变更

### 新增依赖
```toml
[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tokio-util = { version = "0.7", features = ["codec", "io"] }
```

### 移除依赖
无

---

## 4. 文件变更计划

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `Cargo.toml` | 修改 | 添加tracing依赖 |
| `src/main.rs` | 修改 | 添加tracing初始化，kill等待逻辑 |
| `src/server/mod.rs` | 修改 | 日志替换，pty简化(可选) |
| `src/server/daemon.rs` | **重写** | 新架构: 独立PTY任务, JoinSet, CancellationToken |
| `src/server/pty.rs` | 修改 | buffer复用, env::set_var修复 |
| `src/protocol.rs` | 修改 | 添加MAX_PAYLOAD检查 |
| `src/client/attach.rs` | 修改 | BorrowedFd注释, 日志替换 |
| `src/server/backlog.rs` | 修改 | 添加#[must_use], 更多测试 |

---

## 5. 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| 架构改动大 | 可能引入新bug | 每改完一个模块立即cargo check + cargo test |
| forkpty兼容性 | 某些系统可能不工作 | 保留当前实现作为fallback |
| tracing初始化 | daemon stdout已重定向 | 在daemon中配置tracing输出到log文件 |

---

## 6. 验收标准

- [ ] `cargo check` 通过，无clippy警告
- [ ] `cargo test` 通过，测试数量 ≥ 5
- [ ] 协议长度上限生效（可通过测试验证）
- [ ] 客户端数量限制生效
- [ ] graceful shutdown: SIGTERM后客户端正常退出
- [ ] 无新增unsafe代码（除非有充分注释）
- [ ] 所有println/eprintln替换为tracing
