---
task: B4
title: Server module · JSON-RPC over stdio + UDS（D-001 + D-003 + D-004 落地）
date: 2026-05-28
status: draft
depends_on:
  - research/99-decisions D-001 (zhive-server 不独立 crate；zhive-core::server module)
  - research/99-decisions D-003 (Phase 1 RPC = JSON-RPC 2.0 over stdio + UDS；framing 自写住在 zhive-proto)
  - research/99-decisions D-004 (stdio + UDS + Windows lockfile/127.0.0.1 三 transport 并列)
  - research/99-decisions D-008 (反向 RPC 走 JSON-RPC server-initiated request)
  - deliverables/A2-initialize-capabilities.md (method = 裸 `initialize`，protocolVersion u16)
references:
  - ${CODEX}/app-server-transport/src/transport/mod.rs                          (TransportEvent / ConnectionOrigin / OVERLOADED_ERROR_CODE / 路径常量)
  - ${CODEX}/app-server-transport/src/transport/stdio.rs                       (start_stdio_connection；stdin 逐行 + writer mpsc::channel(128))
  - ${CODEX}/app-server-transport/src/transport/unix_socket.rs                 (start_control_socket_acceptor；0o600 chmod；ControlSocketFileGuard Drop 清理)
  - ${CODEX}/app-server-transport/src/outgoing_message.rs                      (OutgoingMessage 四态枚举 + QueuedOutgoingMessage write_complete_tx)
  - ${CODEX}/app-server/src/outgoing_message.rs                                (OutgoingMessageSender + next_server_request_id: AtomicI64)
  - ${CODEX}/app-server/src/message_processor.rs                               (process_request / process_client_request 路由；穷举 match ClientRequest)
  - ${LSP}/src/transport.rs                                                    (Server<I, O, L>.serve()；max_concurrency=4；MESSAGE_QUEUE_SIZE=100)
  - ${LSP}/src/jsonrpc/router.rs                                               (Router<S, E>; HashMap<&'static str, BoxService<…>>; tower 注册式)
  - ${LSP}/src/service/client.rs                                               (Client::next_request_id; AtomicU64 → Id::Number(i64))
  - crates/zhive-proto/src/framing.rs                                          (LSP-style Content-Length；MAX_BODY=16MiB；read_message/write_message 已就位)
  - crates/zhive-proto/src/lib.rs                                              (Message / Request / Response / Notification / Id / ErrorObject)
---

> 范围声明：本 deliverable 仅为 B4 子任务调研产出；**不**包含任何 `crates/` 实现代码改动。
> `${LSP}` = `~/Desktop/code/github/tower-lsp`；`${CODEX}` = `~/Desktop/code/github/codex/codex-rs`。
> 所有 `todo!()` 占位、伪码均为模块草图，**不进 `crates/zhive-core/src/server/`**，由后续 Phase 1 实现 PR 翻译。

---

## 1. 参考点清单

| 论断主题 | 路径 | 行号 / 锚 |
|---|---|---|
| **tower-lsp 仓库末次提交** | `${LSP}/.git` | `49e1ce54 2023-03-15`（"Implement support for client-initiated $/progress"） |
| **tower-lsp crate 版本 + rust-toolchain** | `${LSP}/Cargo.toml` | `version = "0.20.0" / rust-version = "1.64.0"` |
| tower-lsp Router 用 `HashMap<&'static str, BoxService>` 注册法 | `${LSP}/src/jsonrpc/router.rs` | 21-24 / 42-65 |
| tower-lsp `LspServiceBuilder::custom_method` registry 入口 | `${LSP}/src/service.rs` | 216-225 |
| tower-lsp 主循环 `Server::serve()`（max_concurrency=4） | `${LSP}/src/transport.rs` | 22-23 / 101-120 |
| tower-lsp Client 反向 RPC id pool（AtomicU64 → `Id::Number(i64)`） | `${LSP}/src/service/client.rs` | 573-582 |
| codex `TransportEvent` enum（OpenConnection / IncomingMessage / Close） | `${CODEX}/app-server-transport/src/transport/mod.rs` | 163-178 |
| codex `AppServerTransport` enum（Stdio / UnixSocket / WebSocket / Off）—— **无 trait，是 enum** | `${CODEX}/app-server-transport/src/transport/mod.rs` | 66-72 |
| codex `CHANNEL_CAPACITY = 128`（每连接 mpsc bound） | `${CODEX}/app-server-transport/src/transport/mod.rs` | 24 |
| codex stdio：`io::stdin()` BufReader + `lines.next_line()` —— **newline-delimited，不是 Content-Length 框** | `${CODEX}/app-server-transport/src/transport/stdio.rs` | 44-74 |
| codex stdout writer 任务独立 task + writer_rx.recv 循环 | `${CODEX}/app-server-transport/src/transport/stdio.rs` | 82-98 |
| codex UDS 路径常量 + `CONTROL_SOCKET_MODE = 0o600` | `${CODEX}/app-server-transport/src/transport/mod.rs` 46-48 + `transport/unix_socket.rs` | 22 |
| codex `prepare_control_socket_path`：先 connect 探活，再 `is_stale_socket_path` 判定，最后 remove_file | `${CODEX}/app-server-transport/src/transport/unix_socket.rs` | 93-132 |
| codex `ControlSocketFileGuard` Drop 清理 socket 文件 | `${CODEX}/app-server-transport/src/transport/unix_socket.rs` | 174-190 |
| codex 启动锁 `AppServerStartupLock`（`file.lock()` + RAII） | `${CODEX}/app-server-transport/src/transport/unix_socket.rs` | 134-156 |
| codex backpressure：`OVERLOADED_ERROR_CODE = -32001` + `try_send` 满则回 JSON-RPC error | `${CODEX}/app-server-transport/src/transport/mod.rs` | 44 / 222-249 |
| codex 反向 RPC id pool：`next_server_request_id: AtomicI64`（**与 client 完全独立**） | `${CODEX}/app-server/src/outgoing_message.rs` | 97 / 282-284 |
| codex 路由：`match codex_request { ClientRequest::Initialize { .. } ... }` 穷举 match，**不是 registry** | `${CODEX}/app-server/src/message_processor.rs` | 761 / 872-1037 |
| codex `OutgoingMessage` 四态：`Request / AppServerNotification / Response / Error`（`#[serde(untagged)]`） | `${CODEX}/app-server-transport/src/outgoing_message.rs` | 22-31 |
| zhive `read_message` / `write_message` 已就位 + `MAX_BODY=16MiB` | `crates/zhive-proto/src/framing.rs` | 38 / 90 / 163 |
| zhive `Message` enum（Request / Response / Notification） | `crates/zhive-proto/src/lib.rs` | 48-60 |

> **关键差异锚点**：codex stdio 用 **newline-delimited JSON**（line-based），zhive-proto 已落地 **LSP `Content-Length` 框格式**。两者**不通信**——zhive 选 LSP 框是 D-003 + 兼容 ACP/MCP 的明确决策，本 deliverable 沿用，不抄 codex 这一行。

---

## 2. tower-lsp 选型结论（R-3 风险落槌）

| 维度 | 数据 / 锚点 | 结论 |
|---|---|---|
| 末次提交日期 | `49e1ce54 2023-03-15`（约 3 年前） | **陈旧** |
| crate 版本 | `0.20.0`（无 0.21+） | **停滞** |
| `rust-version` | `1.64.0`（zhive 已 1.85+ 时代） | **滞后 ~10 个 Rust 发行** |
| `lsp-types` 依赖 | tower-lsp 锁定 lsp-types 0.94，最新 0.97 | **跟版断开** |
| 替代品 `async-lsp` | crates.io 活跃至 2026-03（lib.rs 数据），tower-based，server+client 对称，notification 同步执行（修正 tower-lsp 已知 bug） | **现代标杆** |
| 替代品 `lsp-server` | rust-analyzer 自带，sync crossbeam-channel + 自写 dispatch loop，2025-08 活跃 | **极简标杆** |

**B4 结论**：

1. **tower-lsp 不能作为 zhive Phase 1 的"抄哪行"对象**——但**可抄其 Router 设计形态**（method registry HashMap + tower Layer），因为该设计已被 `async-lsp` 继承并改进。
2. **正式选型蓝本切换至 `async-lsp`**（不引依赖，仅作设计参考）：
   - 沿用其 `LspService` trait + `MainLoop` 主循环模式
   - 沿用其 "notification 同步、request 并发" 的语义（tower-lsp 的注意点）
   - 沿用其 `&mut self` for requests/notifications + 返回 Future 不借 self 的型签名
3. **codex `app-server-transport` 为工程实测蓝本**（事件驱动 `TransportEvent` + mpsc 背压 + 0o600 UDS + 启动锁）——zhive 直接抄绝大多数工程细节。
4. **形态融合**：codex 的 transport 事件流 + tower-lsp/async-lsp 的 router/method-registry = zhive `Server` 模块的最终形态（详见 §3/§5）。

> R-3 风险结论：tower-lsp **不再作为蓝本**，已切换至 `async-lsp` 设计 + `codex` 工程双蓝本。**不引入 async-lsp 作 dependency**（D-003 + CLAUDE.md 红线 1：禁新增 dep），仅作 design reference。

---

## 3. `Transport` trait + `StdioTransport` / `UdsTransport` 实现要点

### 3.1 设计抉择：trait 还是 enum？

**codex 用 enum** (`AppServerTransport::Stdio | UnixSocket { path } | WebSocket { addr } | Off`)，无 trait。理由：transport 集合是封闭的、由 CLI flag 决定的；事件驱动模型下，每种 transport 只需把 `(connection_id, message, writer_tx)` 推到同一个 `mpsc::Sender<TransportEvent>`，不需要多态 dyn dispatch。

**zhive 选 enum + 抽象 transport 启动函数**——理由：
- D-004 列出 **三种**（stdio / UDS / Windows lockfile+127.0.0.1）已知，无第四种 candidate，封闭枚举更轻
- trait + `dyn Transport` 会要求 `AsyncRead + AsyncWrite + Send + Unpin`，但 UDS 是多连接 acceptor + Windows 是 TCP listener，**底层 IO 形态不同**（单 duplex vs accept loop），强行套同一 trait 会撕裂语义
- `bridge-stdio`（D-010）只用 `io::copy`，不需要 transport 抽象

但**保留 zhive-internal 的 `Transport` 概念**作 **module-level 标识 enum**，配 `serve_*` 一组自由函数（仿 codex `start_stdio_connection` / `start_control_socket_acceptor`）。

### 3.2 草图

```rust
// crates/zhive-core/src/server/transport.rs（草图，不入仓）

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use zhive_proto::{Id, Message};

/// Transport 类型标识（与 codex `AppServerTransport` 对齐）。
///
/// `Lockfile127001` 为 D-004 Windows 第三 transport 占位；Phase 1 **只占接口、不实现**
/// （详见 Q3）。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Transport {
    Stdio,
    Uds { socket_path: PathBuf },
    Lockfile127001 { bind: SocketAddr, lock_path: PathBuf },
    Off,
}

/// 来自 transport 层的事件，由 main loop（§4）单一消费者读取。
///
/// 设计抄 codex `TransportEvent`：`(connection_id, message)` + `(connection_id, writer_tx)`
/// 让 main loop 不关心 transport 形态。
#[derive(Debug)]
pub enum TransportEvent {
    ConnectionOpened {
        connection_id: ConnectionId,
        origin: ConnectionOrigin,
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        disconnect_token: Option<CancellationToken>,
    },
    ConnectionClosed { connection_id: ConnectionId },
    IncomingMessage { connection_id: ConnectionId, message: Message },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConnectionOrigin { Stdio, Uds, Lockfile127001, InProcess }

#[derive(Debug)]
pub struct QueuedOutgoingMessage {
    pub message: OutgoingMessage,
    /// 让发送方观测"已写出"事件（codex 同款）。
    pub write_complete_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

/// 出方向消息（与 codex `OutgoingMessage` 同构）。**Request 是 server-initiated request**
/// （D-008 反向 RPC）。
#[derive(Debug, Clone)]
pub enum OutgoingMessage {
    Request(zhive_proto::Request),
    Notification(zhive_proto::Notification),
    Response(zhive_proto::Response),
    Error { id: Id, error: zhive_proto::ErrorObject },
}
```

### 3.3 `StdioTransport` 实现要点

```rust
// crates/zhive-core/src/server/transport_stdio.rs（草图）

pub async fn start_stdio_connection(
    event_tx: mpsc::Sender<TransportEvent>,
    join_set: &mut tokio::task::JoinSet<()>,
) -> Result<(), ServerError> {
    let connection_id = ConnectionId::next();
    let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
    event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::Stdio,
            writer: writer_tx.clone(),
            disconnect_token: None,
        })
        .await
        .map_err(|_| ServerError::ProcessorUnavailable)?;

    // reader task：用 zhive-proto Content-Length 框（不是 codex 的 line-based）
    let event_tx_r = event_tx.clone();
    let writer_tx_r = writer_tx.clone();
    join_set.spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);
        loop {
            match zhive_proto::framing::read_message(&mut reader).await {
                Ok(msg) => {
                    let event = TransportEvent::IncomingMessage { connection_id, message: msg };
                    if !forward_with_backpressure(&event_tx_r, &writer_tx_r, connection_id, event).await {
                        break; // processor 已关闭
                    }
                }
                Err(zhive_proto::framing::FramingError::UnexpectedEof) => break, // 客户端 EOF
                Err(err) => {
                    tracing::error!(?err, "stdio read error");
                    // 严重 framing 错误 → 关本连接；非 fatal 错误（如 InvalidHeader）下游 reduce
                    break;
                }
            }
        }
        let _ = event_tx_r.send(TransportEvent::ConnectionClosed { connection_id }).await;
    });

    // writer task：单写者，避免 stdout 交错
    join_set.spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(queued) = writer_rx.recv().await {
            let msg: zhive_proto::Message = queued.message.into_proto();
            if let Err(err) = zhive_proto::framing::write_message(&mut stdout, &msg).await {
                tracing::error!(?err, "stdio write error");
                break;
            }
            if let Some(tx) = queued.write_complete_tx { let _ = tx.send(()); }
        }
    });

    Ok(())
}

const CHANNEL_CAPACITY: usize = 128; // 同 codex
```

### 3.4 `UdsTransport` 实现要点

```rust
// crates/zhive-core/src/server/transport_uds.rs（草图）

pub async fn start_uds_acceptor(
    socket_path: PathBuf,
    event_tx: mpsc::Sender<TransportEvent>,
    shutdown: CancellationToken,
) -> Result<tokio::task::JoinHandle<()>, ServerError> {
    prepare_uds_path(&socket_path).await?;       // 见 §6 清理策略
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    set_uds_permissions_0600(&socket_path).await?;
    let guard = UdsFileGuard::new(socket_path.clone()); // Drop 时 remove_file

    Ok(tokio::spawn(async move {
        let _guard = guard; // 保证 acceptor task 结束时清理
        loop {
            let stream = tokio::select! {
                _ = shutdown.cancelled() => break,
                res = listener.accept() => match res {
                    Ok((s, _addr)) => s,
                    Err(err) if is_recoverable(&err) => {
                        tracing::warn!(?err, "recoverable uds accept error");
                        continue;
                    }
                    Err(err) => {
                        tracing::error!(?err, "uds accept fatal");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
            };
            let event_tx = event_tx.clone();
            tokio::spawn(handle_uds_connection(stream, event_tx));
        }
    }))
}

async fn handle_uds_connection(stream: tokio::net::UnixStream, event_tx: mpsc::Sender<TransportEvent>) {
    let connection_id = ConnectionId::next();
    let (reader_half, writer_half) = stream.into_split();
    let (writer_tx, writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);

    // 与 stdio 同结构的两 task；reader 用 read_message，writer 用 write_message
    todo!("see stdio impl; structure identical, only IO handle differs")
}
```

### 3.5 stdio 与 UDS 的区别（Q2 回答）

| 维度 | stdio | UDS |
|---|---|---|
| 单连接 / 多连接 | **单连接**（进程的 stdin/stdout） | **多连接**（acceptor + 每连接一对 task） |
| 启动函数 | `start_stdio_connection` 直接发 `ConnectionOpened` | `start_uds_acceptor` 在 `accept()` 循环里每次发 |
| 关闭 | EOF on stdin → `ConnectionClosed` | client 断开 → `ConnectionClosed`；listener 由 `CancellationToken` 关 |
| 文件资源 | 无（OS handle） | socket 文件 + 启动锁 + 0o600 chmod + Drop 清理 |
| 兼容平台 | 跨平台 | Unix（Windows 走 D-004 第三 transport） |
| **从 main loop 看** | **完全相同**——两端都只是 `TransportEvent` 流入 + `QueuedOutgoingMessage` 流出 |

**结论**：差异**只在 connect / accept 侧**，main loop 与下游 router 完全 transport-agnostic。这是 codex 的核心架构红利，B4 直接继承。

---

## 4. 事件循环 main loop 伪码

```rust
// crates/zhive-core/src/server/main_loop.rs（草图）

/// 与 codex `MessageProcessor::run()` 同构；与 async-lsp `MainLoop` 概念同构。
pub async fn run_server(
    transport: Transport,
    router: Arc<RequestRouter>,
    outgoing: Arc<OutgoingMessageSender>,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    // 单 main mpsc：所有 transport 把事件汇聚到这里
    let (event_tx, mut event_rx) = mpsc::channel::<TransportEvent>(MAIN_QUEUE_CAPACITY);
    let mut join_set = JoinSet::new();

    // 1. 拉起 transport
    match transport {
        Transport::Stdio => start_stdio_connection(event_tx.clone(), &mut join_set).await?,
        Transport::Uds { socket_path } => {
            let handle = start_uds_acceptor(socket_path, event_tx.clone(), shutdown.clone()).await?;
            join_set.spawn(async move { let _ = handle.await; });
        }
        Transport::Lockfile127001 { .. } => return Err(ServerError::TransportNotImplementedInPhase1),
        Transport::Off => {} // 仅用于 unit test
    }

    // 2. 主分发循环
    let mut connections: HashMap<ConnectionId, ConnectionState> = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(event) = event_rx.recv() => match event {
                TransportEvent::ConnectionOpened { connection_id, origin, writer, .. } => {
                    connections.insert(connection_id, ConnectionState { origin, writer });
                }
                TransportEvent::ConnectionClosed { connection_id } => {
                    if let Some(state) = connections.remove(&connection_id) {
                        outgoing.abort_pending_server_requests_for(connection_id).await; // §5
                    }
                }
                TransportEvent::IncomingMessage { connection_id, message } => {
                    let conn = match connections.get(&connection_id) {
                        Some(c) => c.clone(),
                        None => continue, // 已断开
                    };
                    let router = router.clone();
                    let outgoing = outgoing.clone();
                    // 每个 incoming 单独 spawn，避免 head-of-line block
                    // max_in_flight 由 outgoing 的有界 mpsc + transport try_send overload 协同（Q6）
                    tokio::spawn(async move {
                        match message {
                            Message::Request(req) => {
                                let id = req.id.clone();
                                match router.dispatch(req, connection_id).await {
                                    Ok(value) => outgoing.send_response(connection_id, id, value).await,
                                    Err(err) => outgoing.send_error(connection_id, id, err).await,
                                }
                            }
                            Message::Notification(n) => router.dispatch_notification(n, connection_id).await,
                            Message::Response(resp) => outgoing.notify_client_response(resp).await, // 反向 RPC 回调
                        }
                    });
                }
            }
        }
    }

    // 3. 退出：drain JoinSet
    while let Some(_) = join_set.join_next().await {}
    Ok(())
}

const MAIN_QUEUE_CAPACITY: usize = 256; // 比每连接 128 大一档
```

> 对比 tower-lsp `Server::serve()` 用 `buffer_unordered(max_concurrency=4)` 限制并发——zhive **不在 main loop 限并发**，而把 backpressure 推到 transport 层 `try_send`（更接近 codex 做法，规避 tower-lsp 已知的 notification 乱序 bug）。
>
> 对比 async-lsp：async-lsp 把 cancellation / concurrency 做成 tower Layer 注入 router。zhive 后续若引入 layer 体系（Phase 2+ 加 tracing/permission middleware）再补，Phase 1 不上 tower 依赖。

---

## 5. 反向 RPC id 池设计（Q5 回答）

### 5.1 三种方案对比

| 方案 | 形态 | 优点 | 缺点 |
|---|---|---|---|
| A：共享 id pool | 单 `AtomicU64`，client→server 与 server→client 同源 | 实现最简 | 双方需协调起点；client 不知道 server 用了哪些 id 会造成 id 碰撞（id 是字符串/整数 + 单调，但 client 可能 reuse 之前服务端使用过的整数） |
| B：完全分离 pool | server 用 `AtomicI64` 自增（**codex 模式**） | id 责任清晰；服务端不需要知道 client 的 id 习惯；锚 outgoing_message.rs L97 L282-284 | **可能 wire 上同一时刻出现两条 id=7 的 request**（client→server 一条 + server→client 一条）——但 JSON-RPC 2.0 spec 允许：id 唯一性只在 **同向** 内有效（不同方向独立） |
| C：区段隔离 | client→server 用 `[1, 2^31)`，server→client 用 `[2^31, 2^32)`（或负数段） | wire 上 id 全局唯一；调试友好（看 id 立刻知方向） | 设计成本最高；与 LSP/ACP/codex 生态字面不兼容；client 必须遵守"我不用 >= 2^31 的 id" |

### 5.2 zhive 决策：**方案 B（完全分离）**

**抄 codex**：`OutgoingMessageSender { next_server_request_id: AtomicI64, ... }`（锚 `${CODEX}/app-server/src/outgoing_message.rs:97 / 282-284`），与 client→server 的 id 完全独立。

```rust
pub struct OutgoingMessageSender {
    next_server_request_id: AtomicU64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    /// 反向 RPC pending callbacks：server 发的 request 在此等 client 的 response
    pending_callbacks: Mutex<HashMap<Id, PendingCallback>>,
    /// 入方向 request 的上下文：用于在 client 断开时清理未回应的 request
    pending_inbound: Mutex<HashMap<(ConnectionId, Id), InboundContext>>,
}

impl OutgoingMessageSender {
    fn next_server_request_id(&self) -> Id {
        Id::Number(self.next_server_request_id.fetch_add(1, Ordering::Relaxed) as i64)
    }

    pub async fn send_request(
        &self,
        connection_id: ConnectionId,
        method: String,
        params: Option<serde_json::Value>,
    ) -> (Id, oneshot::Receiver<Result<serde_json::Value, zhive_proto::ErrorObject>>) {
        let id = self.next_server_request_id();
        let (tx, rx) = oneshot::channel();
        self.pending_callbacks.lock().await.insert(id.clone(), PendingCallback { tx, connection_id });
        // 通过 transport writer_tx 发出
        let _ = self.sender.send(OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(zhive_proto::Request::new(id.clone(), method, params)),
            write_complete_tx: None,
        }).await;
        (id, rx)
    }
}
```

**与 tower-lsp 对比**：tower-lsp 同样**完全分离**——`Client::next_request_id` 是独立 `AtomicU64`（锚 `${LSP}/src/service/client.rs:578-582`）。**业界共识**。

**未决项 → TODO(开放项 B4-1)**：连接断开时，pending_callbacks 里挂在该 connection_id 上的 callback 必须立即返回 `ConnectionClosed` 错误，否则 hook host / permission reducer 的 await 会卡死。codex 用 `abort_pending_server_requests_for(connection_id)`（锚 `${CODEX}/app-server/src/outgoing_message.rs:176`），B4 直抄。

---

## 6. UDS socket 路径 / 权限 / 清理策略

绑定 D-004 决策：默认 `$XDG_RUNTIME_DIR/zhive.sock`，权限 0600。

### 6.1 路径选择

```rust
fn default_uds_socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("zhive.sock")
    } else {
        // tmpfs fallback：/tmp/zhive-<uid>.sock
        // 用 uid 隔离同机多用户；通过 fchmod 0600 + 父目录不可写也能挡同用户其他进程
        let uid = unsafe { libc::getuid() }; // CLAUDE.md 红线：unsafe 需批准 → TODO(B4-2)
        PathBuf::from(format!("/tmp/zhive-{uid}.sock"))
    }
}
```

> **TODO(开放项 B4-2)**：`libc::getuid()` 是 unsafe，违反 CLAUDE.md 红线 2。替代方案：用 `rustix` crate（已成熟、纯 safe wrapper），但属新增依赖（红线 1）；或仅依赖 `XDG_RUNTIME_DIR`，没有则报错让用户配置。Phase 1 推荐**后者**（拒 fallback），减少依赖且语义清晰。

### 6.2 权限设置

抄 codex（锚 `unix_socket.rs:158-167`）：

```rust
#[cfg(unix)]
async fn set_uds_permissions_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
}
```

**陷阱**：`UnixListener::bind` 是 atomic create + bind，但**创建时的权限由 umask 决定**——如果用户 umask 是 0022，新建的 socket 默认是 0755。必须**先 bind 再立即 chmod**（即上面顺序）。codex 这么做。一种更安全的方式是先把父目录设为 0700（codex `prepare_private_socket_directory` 走这条），再在内部建 socket，即便短暂窗口期权限放宽，外部进程也进不来——zhive 可两层都做。

### 6.3 清理策略（stale socket）

抄 codex `prepare_control_socket_path`（锚 `unix_socket.rs:93-132`）：

```rust
async fn prepare_uds_path(path: &Path) -> std::io::Result<()> {
    // step 1: 父目录 0700
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        set_dir_permissions_0700(parent).await?;
    }

    // step 2: 试 connect 旧 socket。如果连得上 → 已有 server 在跑，报错退出
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("zhive server already running at {}", path.display())
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),         // 没文件，干净
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {}            // 文件在但没人接 = stale
        Err(err) => {
            // 其他错误：再做一次 try_exists，给路径上"非 socket 文件"留拒绝路径
            if !path.try_exists()? { return Ok(()); }
            return Err(err);
        }
    }

    // step 3: 验证是 socket 文件（不是普通文件），然后 unlink
    if !is_socket(path).await? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("path exists but is not a socket: {}", path.display())
        ));
    }
    tokio::fs::remove_file(path).await
}
```

### 6.4 启动锁（避免 race）

抄 codex `AppServerStartupLock`（锚 `unix_socket.rs:134-156`）：在 `$XDG_RUNTIME_DIR/zhive-startup.lock` 文件上 `flock()`。两个进程同时启动时，第二个 `lock()` 会阻塞或失败。RAII guard 让锁随进程结束自然释放。

```rust
pub struct ServerStartupLock { _file: std::fs::File }
pub async fn acquire_startup_lock(path: PathBuf) -> std::io::Result<ServerStartupLock> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
        file.lock()?; // std::fs::File::lock 已稳定（Rust 1.83+）
        Ok(ServerStartupLock { _file: file })
    }).await.map_err(io::Error::other)?
}
```

### 6.5 关闭时清理（Drop 守卫）

抄 codex `ControlSocketFileGuard`（锚 `unix_socket.rs:174-190`）：

```rust
struct UdsFileGuard { path: PathBuf }
impl Drop for UdsFileGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), %err, "failed to remove uds socket file");
            }
        }
    }
}
```

---

## 7. 关键问题逐条作答

### Q1：`Transport` trait 接口（`AsyncRead + AsyncWrite` 还是更高层 message-stream？）

**答**：**更高层 message-stream（事件 mpsc）**，且 zhive 选 **enum + 自由函数**，不是 trait。

- codex 实测就是事件 mpsc：`TransportEvent` 推到主 channel（锚 mod.rs L163-178），上层从不看 raw bytes
- tower-lsp 用 `Server<I: AsyncRead, O: AsyncWrite>`（锚 transport.rs L59-66）——但**只支持单连接 stdio**，多连接 UDS 用不上这层抽象；这是 tower-lsp 2022 设计的局限
- zhive 选 codex 模式：transport 内部用 `AsyncRead/Write`（通过 zhive-proto `framing::read_message/write_message`），但**对外只暴露 mpsc 事件流**
- 选 enum 而非 trait 的理由见 §3.1（封闭集合 + 不同 IO 形态）

### Q2：stdio transport 与 UDS transport 区别只在 connect 侧还是更深？

**答**：**只在 connect/accept 侧**。详见 §3.5 对照表。Main loop 与 router 完全 transport-agnostic。

### Q3：Windows 第三 transport（lockfile + 127.0.0.1，D-004 决策）是否进 Phase 1 实现还是只占接口？

**答**：**只占接口（enum 变体 `Transport::Lockfile127001`），Phase 1 不实现**。

理由：
- D-001 说 7 个 crate 起步，Phase 1 工程预算极紧
- D-004 字面要求"平行存在"但**未要求 Phase 1 全部落地**——同 D-010 "Phase 2 生态接入"分期逻辑
- Windows 实测客户少（zhive 主目标 Linux/macOS 开发者）；用户在 Windows 上能用 `wsl` 起 zhive 走 UDS
- 留 enum 变体 + `ServerError::TransportNotImplementedInPhase1` 让 CLI flag 解析能识别但运行时拒绝，避免 user 误以为支持后报 framing 错误
- 实现该 transport 的工程量：~150-250 行（TCP listener bind 到 127.0.0.1:0 + lockfile 写 port + 启动时 token 鉴权防 localhost 同机其他用户），比 UDS 复杂；放 Phase 2 是合理切片

**TODO(开放项 B4-3)**：D-004 是否需要在 Phase 1 出 CLI flag `--transport windows-lockfile` 占位？建议**出 flag、运行时报"Phase 2 ready"错误**，给 Windows 用户提前看到迁移信号。

### Q4：请求路由（method name → handler）：硬编码 match 还是 registry？tower-lsp 怎么做？

**答**：**zhive 选硬编码 enum match（codex 模式），不抄 tower-lsp registry**。

- **tower-lsp / async-lsp 都是 registry**：`Router<S, E> { methods: HashMap<&'static str, BoxService<...>> }`（锚 `${LSP}/src/jsonrpc/router.rs:21-24`），通过 builder `custom_method(name, callback)` 注册（锚 `${LSP}/src/service.rs:216-225`）
- **codex 是穷举 match**：`match codex_request { ClientRequest::Initialize { .. } => ..., ClientRequest::ConfigRead { .. } => ..., ... }`（锚 `${CODEX}/app-server/src/message_processor.rs:872-1037`，单一 match 覆盖 50+ 变体）
- zhive 选 match 的理由：
  1. zhive-proto 的 `Message::Request` 反序列化时 method 字符串需先解析成 enum（A2 的 `ClientRequest` enum 草案）；既然 enum 已存在，match 是自然形态
  2. registry 的 dyn dispatch + `Box<dyn Service>` 会引入 1-2µs 间接调用 + 失去 method-level 类型安全（params 拿到的是 `serde_json::Value`，handler 内部还得 deserialize 一次）
  3. zhive Phase 1 方法数估计 < 30，单 match 函数 < 200 行可控
  4. CLAUDE.md "单函数 > 80 行需要说明" —— match 自然按 method group 拆分（thread/* / turn/* / permission/* / hook/*），每个 sub-router 函数 < 80 行
  5. registry 的真正优势是"插件式注册扩展方法"——zhive 走 D-013 extension manifest 体系，不在 wire-level method 字符串这层做扩展
- **抄 tower-lsp 的部分**：method group 拆分思路 + 错误响应字段（`-32601 MethodNotFound`、`-32602 InvalidParams`）

**TODO(开放项 B4-4)**：A2 的 `ClientRequest` enum 在哪定义？是 zhive-proto 还是 zhive-core？影响 router 的 import 拓扑。建议**放 zhive-proto**，避免 bridge-stdio 反向依赖 core。

### Q5：反向 RPC（server-initiated request）的 id 空间——与 client → server 共享 id pool 还是分离？

**答**：**完全分离**（方案 B）。详见 §5。codex + tower-lsp 双重背书。

### Q6：backpressure——客户端发太快怎么办？

**答**：**三层防御，抄 codex**。

1. **每连接 mpsc 有界**：`CHANNEL_CAPACITY = 128`（codex 同值）。reader task `try_send` 到 main event channel，**满了不阻塞**——直接对该 request 回 `-32001 Overloaded` 错误，让客户端自己重试或限流（锚 `${CODEX}/app-server-transport/src/transport/mod.rs:222-249`）。
2. **Notification 的特殊处理**：notification 没有 id 不能回错误，只能丢弃 + warning log（codex 同样选择，锚 mod.rs L248）。在 zhive 里通过 `tracing::warn!` 上报，由 D-014 的 OTel pipeline 监控。
3. **Response 必走 await**：response 是对 server-initiated request 的回复，如果队列满直接丢就会让 pending_callback 永远不返回 —— 必须 `await` 等队列让出（codex 在 mod.rs L248 同样把 response/notification 走 `send().await`）。

**zhive 决策**：
- **Request**：`try_send`，满则回 `-32001 ServerOverloaded`（错误 data 字段带 `{ retry_after_ms: 100 }` 建议）
- **Notification**：`try_send`，满则丢 + tracing warn
- **Response**：`send().await`，阻塞，因为丢失会破坏 D-008 反向 RPC 语义
- **错误码**：`-32001` 与 codex 撞——**不冲突**，因 JSON-RPC spec 把 `-32000..-32099` 留作"服务端 implementation-defined"，zhive 独立编号空间。建议 zhive 用 `-32010` 起步避开 codex 显眼号，留前面 9 个号给真正的 JSON-RPC framing 错误（`-32007 FrameTooLarge` 等）。**TODO(开放项 B4-5)**：错误码编号方案需在 A2 的 `ErrorObject.code` 决策里固化。

---

## 8. 与 tower-lsp 的并列对照（抄哪行 / 不抄哪行）

| tower-lsp 做法 | zhive 取舍 | 理由 |
|---|---|---|
| `Server<I: AsyncRead, O: AsyncWrite>` 单连接 stdio 抽象 | ❌ 不抄 | 多连接 UDS 用不上；事件驱动 mpsc 更通用 |
| `Router<S, E>` + `HashMap<&'static str, BoxService<...>>` registry | ❌ 不抄 router | 见 Q4；zhive 选 enum match。但**抄 method-group 拆分思路** |
| `Server::serve()` 的 `buffer_unordered(max_concurrency=4)` 并发限制 | ❌ 不抄 | 强加并发限制会与 D-008 反向 RPC + permission reducer 的"取消传播"打架；选 codex 的 backpressure 模型 |
| `Client::next_request_id` 用 `AtomicU64` 分离 id pool | ✅ 抄 | 与 codex 同；见 §5 / Q5 |
| `Client::send_request` 用 oneshot::channel 回 pending callback | ✅ 抄设计 | codex 同形态（pending_callbacks: `Mutex<HashMap<Id, oneshot::Sender>>`） |
| `LanguageServer` trait + `#[async_trait]` 的 handler trait | ❌ 不抄 trait | zhive 用具体类型 `Server { router, outgoing, ... }`，handler 是 method 函数；trait 过度抽象 |
| `LspServiceBuilder::custom_method` 注册自定义 method | ❌ 不抄 | 见上一行 |
| tower-lsp 的 `notification` 异步执行（已知 bug） | ❌ 反例 | async-lsp 修正为 notification 同步执行；zhive 抄 async-lsp 修正版 |
| tower-lsp 的 `$/cancelRequest` 半内置支持 | ⚠️ 部分抄概念 | zhive 用 D-008 的 `turn/cancel`，不抄 LSP `$/cancelRequest` 字符串 |
| tower-lsp `LanguageServerCodec`（Content-Length 框格式 codec） | ✅ 抄格式 | zhive-proto framing 已落地此格式（不依赖 tower-lsp） |

**alternative 候选切换建议**：

| 候选 | 是否引入依赖 | 价值 |
|---|---|---|
| `async-lsp` | ❌ 不引（D-003 / 红线 1） | 仅作设计参考：MainLoop 形态、notification 同步、middleware-by-layer |
| `lsp-server` | ❌ 不引 | 太薄（不带 router 不带 client 反向 RPC），不如直接抄 codex |
| `jsonrpsee` | ❌ 不引（不支持 stdio，issue #5 已确认） | 无价值 |

---

## 9. 未决项汇总（TODO）

1. **TODO(开放项 B4-1)**：UDS / stdio 连接断开时，挂在该 connection 上的反向 RPC pending_callbacks 必须批量解绑（返回 `ConnectionClosed` 错误）。codex `abort_pending_server_requests_for` 直接抄即可，但需要在 zhive `OutgoingMessageSender` 加 `connection_id → Vec<Id>` 二级索引。
2. **TODO(开放项 B4-2)**：`/tmp/zhive-<uid>.sock` 回退路径需要 `getuid()`，触发 CLAUDE.md unsafe 红线；建议 Phase 1 强制要求 `XDG_RUNTIME_DIR`，没有则报错；Phase 2 加 `rustix` 依赖（走 PR 审批）。
3. **TODO(开放项 B4-3)**：D-004 的 Windows 第三 transport（lockfile + 127.0.0.1）—— Phase 1 是否暴露 CLI flag 占位？建议**暴露 flag、运行时报 `TransportNotImplementedInPhase1` 错误**，给 Windows 用户提前迁移信号。
4. **TODO(开放项 B4-4)**：A2 落定的 `ClientRequest` enum 在 zhive-proto 还是 zhive-core？影响 router 的 import 方向 + bridge-stdio 的依赖图。建议**放 zhive-proto**。
5. **TODO(开放项 B4-5)**：JSON-RPC 错误码编号方案需固化：`-32001 ServerOverloaded` 是抄 codex 还是另起？建议 zhive 从 `-32010` 起编号自身错误，前面留给框架级（framing/parse 失败）。
6. **TODO(开放项 B4-6)**：tower-lsp `proposed` feature 引入的 `$/progress` 半流式机制，zhive 是否需要对齐？D-008 `streaming_behavior` 已覆盖大部分场景，但单 turn 内的"短期 progress notification"是否单独立 method 仍未决。
7. **TODO(开放项 B4-7)**：tracing span 在 main loop / transport 层的注入点——D-014 要求 `Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint` 强制覆盖，**transport 层是否也强制 span**（如 `transport.message_in` / `transport.message_out`）未决；建议加，便于排查 backpressure 问题。

---

## 10. R-3 风险结论

**R-3 plan §9 风险落槌**：tower-lsp 2022 最后 release / 2023-03 最后 commit，**已不代表"现代标杆"**。

- ✅ 抄概念形态（Router HashMap registry / Client reverse-id-pool / Content-Length codec）
- ❌ 不抄 trait 设计（`LanguageServer` 太重，notification async 是 bug）
- ❌ 不引为 dependency（红线 1 + 跟版断开）

**新蓝本**：
1. **codex `app-server-transport`**（工程实测）—— 抄 `TransportEvent` mpsc + 0o600 UDS + 启动锁 + Drop 清理
2. **async-lsp 设计**（理念参考）—— 抄 notification 同步执行 + Service trait by tower Layer 概念（**不引依赖**）
3. **zhive-proto framing**（已就位）—— 直接复用 LSP Content-Length 编解码

**R-3 残余风险**：无重大残余。若 Phase 2+ 引入 middleware 体系（permission reducer / tracing 自动注入），届时再评估是否引 async-lsp 作 design library（仍不引 runtime）。

---
