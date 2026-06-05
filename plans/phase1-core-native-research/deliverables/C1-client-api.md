---
task: C1
plan: phase1-core-native-research
date: 2026-05-28
status: implemented
crate: zhive-client-native（仅依赖 zhive-proto；不依赖 zhive-core）
depends_on:
  - deliverables/A1-thread-turn-item.md   (Thread / Turn / Item / TurnStarted/CompletedNotification)
  - deliverables/A2-initialize-capabilities.md (InitializeRequest / InitializeResponse / Capabilities / ProtocolVersion / `initialized` notification)
  - deliverables/A3-permission-streaming-subagent.md (`permission/request` reverse-RPC，`Cancelled` outcome)
references:
  - ${CODEX}/app-server-client/src/lib.rs                 (InProcessAppServerClient + InProcessClientStartArgs + AppServerClient enum + AppServerEvent + TypedRequestError + AppServerRequestHandle)
  - ${CODEX}/app-server-client/src/remote.rs              (RemoteAppServerClient + RemoteAppServerEndpoint + RemoteAppServerConnectArgs + initialize_remote_connection)
  - ${CODEX}/app-server-client/Cargo.toml                 (deps：codex-app-server-protocol / tokio / tokio-tungstenite / url)
  - crates/zhive-client-native/src/lib.rs                 (Phase 1 起点，仅 `version()`，无 Client/Builder)
  - crates/zhive-client-native/Cargo.toml                 (已仅依赖 zhive-proto + tokio + async-trait + futures + thiserror)
non-goals:
  - 不写 zhive crate 源码（本 deliverable 内 Rust 代码块全部 `todo!()` 占位）
  - 不改 research/99-decisions/
  - 不暴露 zhive-core 类型（client 仅依赖 zhive-proto）
---

> 范围声明：本文件是 C1 调研产出。所有 Rust 代码段为 deliverable 内**草图**，全部 `todo!()` 占位，不进 `crates/`。
> ${CODEX} = `~/Desktop/code/github/codex/codex-rs/`，本调研只读 codex `app-server-client/{lib.rs, remote.rs, Cargo.toml}` 三个文件（≤4 上限）。
> **client 仅消费 zhive-proto 的类型**：Thread / Turn / Item / Initialize* / Capabilities / ProtocolVersion / `permission/*` payload / JSON-RPC envelope。它不知道 `zhive-core::Engine`，也不知道 `Provider` / `HookHost` —— 这些是 server 侧实现细节。

---

## 1. 参考点清单

| 主题 | 路径 | 行号 |
|---|---|---|
| codex `InProcessAppServerClient` 主结构 + 字段 | `${CODEX}/app-server-client/src/lib.rs` | 463-467 |
| codex `InProcessAppServerClient::start(args)` 入口 | `${CODEX}/app-server-client/src/lib.rs` | 485-608 |
| codex `InProcessClientStartArgs` 字段（含 `client_name / client_version / experimental_api / opt_out_notification_methods / channel_capacity`） | `${CODEX}/app-server-client/src/lib.rs` | 330-368 |
| codex `InitializeParams` 由 `start_args.initialize_params()` 内建（client 内部组装握手 payload，不暴露给 caller） | `${CODEX}/app-server-client/src/lib.rs` | 377-398 |
| codex `request(ClientRequest) -> IoResult<RequestResult>`（裸 JSON-RPC result） | `${CODEX}/app-server-client/src/lib.rs` | 620-640 |
| codex `request_typed::<T>(ClientRequest) -> Result<T, TypedRequestError>` | `${CODEX}/app-server-client/src/lib.rs` | 648-666 |
| codex `notify(ClientNotification)` | `${CODEX}/app-server-client/src/lib.rs` | 669-689 |
| codex `resolve_server_request(req_id, JsonRpcResult)` + `reject_server_request(req_id, JSONRPCErrorError)`（反向 RPC 应答的唯二入口） | `${CODEX}/app-server-client/src/lib.rs` | 695-748 |
| codex `next_event() -> Option<InProcessServerEvent>`（单一事件流：含 ServerNotification + ServerRequest + Lagged） | `${CODEX}/app-server-client/src/lib.rs` | 750-757 |
| codex `AppServerEvent` enum（融合 Lagged / ServerNotification / ServerRequest / Disconnected 四 case） | `${CODEX}/app-server-client/src/lib.rs` | 131-149 |
| codex `AppServerClient` 顶层 enum（`InProcess(...) \| Remote(...)`，dispatch 各 method） | `${CODEX}/app-server-client/src/lib.rs` | 480-483, 861-928 |
| codex `AppServerRequestHandle`（可 `Clone` 的请求句柄，无事件接收能力；用于多 task 并发 request） | `${CODEX}/app-server-client/src/lib.rs` | 469-478, 842-859 |
| codex `TypedRequestError` 三态（Transport / Server / Deserialize） | `${CODEX}/app-server-client/src/lib.rs` | 280-328 |
| codex `RemoteAppServerEndpoint` 二态（WebSocket / UnixSocket）+ `RemoteAppServerConnectArgs` | `${CODEX}/app-server-client/src/remote.rs` | 72-91 |
| codex `RemoteAppServerClient::connect(args)` 入口（**与 InProcess 不共用 builder**） | `${CODEX}/app-server-client/src/remote.rs` | 163-182 |
| codex `initialize_remote_connection(...)` 内部完成 `initialize` 请求 + 等响应 + 收尾 `initialized` notification | `${CODEX}/app-server-client/src/remote.rs` | 798-933 |
| codex `Disconnected { message }` 是 `AppServerEvent` 一态（不 panic、不 abort） | `${CODEX}/app-server-client/src/lib.rs` | 136 |
| codex `shutdown(self) -> IoResult<()>`（消费 self；带 `SHUTDOWN_TIMEOUT=5s` 兜底 abort） | `${CODEX}/app-server-client/src/lib.rs` | 122, 763-795 |
| codex `Cargo.toml` 依赖项（tokio-tungstenite / url / codex-app-server-protocol，不依赖 codex-core 类型 …… 等等，**实际依赖 codex-core**：lib.rs:51 `use codex_core::config::Config`） | `${CODEX}/app-server-client/Cargo.toml` | 14-26 |
| zhive client 当前状态：仅 `version()`，无 Client/Builder | `crates/zhive-client-native/src/lib.rs` | 1-18 |
| zhive client Cargo.toml 已就位的 dep：仅 `zhive-proto + tokio + async-trait + futures + anyhow + thiserror + tracing + bytes + serde_json` | `crates/zhive-client-native/Cargo.toml` | 16-25 |
| A1 `Thread / Turn / Item / TurnStartedNotification / TurnCompletedNotification`（client 要 expose） | `deliverables/A1-thread-turn-item.md` §6 | — |
| A2 `InitializeRequest / InitializeResponse / Capabilities / ProtocolVersion`（client builder 要消费） | `deliverables/A2-initialize-capabilities.md` §2-§3 | — |
| A3 `permission/request` reverse-RPC + `Cancelled` outcome（client 要给 caller 一个 handler 注册口） | `deliverables/A3-permission-streaming-subagent.md` §6.3 | — |

---

## 2. `Client / ClientBuilder` 公开 API 草图

### 2.1 设计 invariant

1. **单一公开类型 `Client`**：底层 transport（stdio / uds / remote/ws）通过 `enum` 内部 dispatch，对 caller 透明。**对齐 codex `AppServerClient` enum**（lib.rs:480-483）。
2. **`ClientBuilder` 是构造器**：caller 用 fluent setter 链组装 `Implementation` / `Capabilities` / `protocol_version` / `channel_capacity`，**与 transport 选择正交**。三种 `connect_*` 方法都是 builder 的 **终态消费方法**。⇒ 三 connect 共享同一 builder（C1-Q1 答案）。
3. **同步与流式分离**：caller 走 `client.request(...).await` 拿 typed response；caller 单独 `loop { client.next_event().await }` 消费 `ClientEvent` 流。**对齐 codex**（codex 把 typed request 和 `next_event` 拆成两个方法，分别走两条 channel）。**不**做 `subscribe(method) -> Stream<Notification>` 单 method 订阅 —— 那是 LSP-style，codex 没采用，徒增 API 表面（C1-Q2 答案）。
4. **Reverse-request 由 `trait ReverseHandler` 处理**：caller 在 builder 上 `.reverse_handler(Arc<dyn ReverseHandler>)` 注册一个对象；该 trait 由 client 内 worker 在每条入站 `ServerRequest` 上调用。也接受降级 fallback：未注册时所有反向 request 自动用 `MethodNotFound` 失败（不 panic）（C1-Q3 答案）。
5. **Drop / 连接异常**：worker 把异常落到 `ClientEvent::Disconnected { message }` 一态，**不 panic、不 abort 进程**。`Drop` impl 仅 best-effort 关闭 channel；显式 `shutdown()` 是 graceful path（C1-Q4 答案）。

### 2.2 Rust 草图（zhive-client-native 公开 API surface）

```rust
//! Phase 1 草图：zhive-client-native 公开 API surface。
//!
//! 仅依赖 zhive-proto。不依赖 zhive-core。

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Child;
use serde::de::DeserializeOwned;

// 仅从 zhive-proto 引入；client 不知道 zhive-core 任何类型
use zhive_proto::{
    domain::{Thread, ThreadId, Turn, TurnId},            // A1
    handshake::{Capabilities, Implementation, ProtocolVersion}, // A2
    permission::PermissionDecision,                      // A3
    JsonRpcError, RequestId, Notification, ServerRequest,
};

// ========================================================
// Error 类型（对齐 codex `TypedRequestError`，分 transport / server / decode 三层）
// ========================================================

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("{method} transport error: {source}")]
    Transport {
        method: String,
        #[source] source: std::io::Error,
    },
    /// 对端返回结构化 JSON-RPC error（含 code/message/data）。
    /// data 字段在 A2 协商失败时携带 `{ supported: [..], requested: V99 }`。
    #[error("{method} failed: code={} msg={}", source.code, source.message)]
    Server {
        method: String,
        #[source] source: JsonRpcError,
    },
    #[error("{method} response decode error: {source}")]
    Deserialize {
        method: String,
        #[source] source: serde_json::Error,
    },
    /// Builder 阶段 invariant 违反（protocol_version=0、空 client_name 等）
    #[error("client configuration error: {0}")]
    Config(String),
    /// 连接已断开；后续调用都会立即返回此错。worker task 已退出。
    #[error("client disconnected: {0}")]
    Disconnected(String),
}

// ========================================================
// Reverse-RPC handler trait（caller 实现，client worker 调用）
// ========================================================

/// Server → Client 反向请求处理器。
///
/// Worker 在每条入站 `ServerRequest` 上调一次 `handle`。**该 trait 不暴露
/// `JsonRpcResult` 裸类型**——`handle` 返回 `Result<serde_json::Value, JsonRpcError>`
/// 由 client 自动包成 wire 应答。
///
/// 已注册的方法名通过 `methods()` 声明；worker 在 dispatch 前先按 method 过滤，
/// 未声明的 method 走 `MethodNotFound` 应答（不调 `handle`）。
///
/// 典型实现：
/// - `permission/request` → caller 弹 UI → 返回 `PermissionDecision` 序列化的 JSON
/// - `hook/run` → caller 跑用户 hook 脚本（A4 + B5）
#[async_trait::async_trait]
pub trait ReverseHandler: Send + Sync {
    /// 此 handler 声明能处理的 method 列表（worker 用来 fast-path 拒绝未知方法）。
    fn methods(&self) -> &[&'static str];

    /// 处理一条反向请求。返回 `Ok(value)` ⇒ client 自动构造 `{"result": value}` 应答；
    /// 返回 `Err(JsonRpcError)` ⇒ client 自动构造 `{"error": ...}` 应答。
    ///
    /// **必须**在合理时间内返回（建议 ≤ 30s，与 A3 §10 TODO-A3-O3 timeout 对齐）；
    /// 若 client 的 `session/cancel` 已触发，worker 会**先用 `Cancelled` outcome
    /// 自动应答所有 in-flight** 然后才放弃 `handle` future（A3 §6.2 决策）。
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, JsonRpcError>;
}

// ========================================================
// 事件流：所有 server→client 的非应答类消息（融合 Notification + ServerRequest + 系统态）
// 对齐 codex `AppServerEvent` 四 case（Lagged / ServerNotification / ServerRequest / Disconnected）。
// ========================================================

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ClientEvent {
    /// 服务端发来的 notification（如 `events/turn_started` / `events/session_aborted`）。
    Notification(Notification),
    /// 服务端发来的反向请求。**caller 不需要自己应答**——已注册 `ReverseHandler`
    /// 的方法由 worker 自动 dispatch；此 case 仅在 caller 想旁路观测时用。
    /// 若没有匹配的 handler，worker 在 emit 此事件**之前**已用 `MethodNotFound` 应答了。
    ServerRequest(ServerRequest),
    /// Backpressure：consumer 落后，期间 best-effort 通知丢了 `skipped` 条。
    /// Lossless 通知（`events/turn_completed` / `events/item_appended` 等）会阻塞通道而不是落入此 case，
    /// 与 codex `event_requires_delivery` 同语义（lib.rs:151-186）。
    Lagged { skipped: usize },
    /// 连接断开。**到达此事件后所有 `request()` 立即返回 `ClientError::Disconnected`**；
    /// worker task 已退出，不会再有事件。
    Disconnected { message: String },
}

// ========================================================
// Builder（三 connect 共享）
// ========================================================

pub struct ClientBuilder {
    client_info: Option<Implementation>,
    capabilities: Capabilities,
    protocol_version: ProtocolVersion,
    channel_capacity: usize,
    reverse_handler: Option<Arc<dyn ReverseHandler>>,
    initialize_timeout: Duration,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            client_info: None,
            capabilities: Capabilities::default(),
            protocol_version: ProtocolVersion::LATEST,
            channel_capacity: 64,
            reverse_handler: None,
            initialize_timeout: Duration::from_secs(10),
        }
    }
}

impl ClientBuilder {
    pub fn new() -> Self { Self::default() }

    /// 必填。对齐 A2 `Implementation { name, title, version }`。
    pub fn client_info(mut self, info: Implementation) -> Self { self.client_info = Some(info); self }

    /// 可选。默认 `Capabilities::default()`（cancellation=true，其余 false）。
    pub fn capabilities(mut self, caps: Capabilities) -> Self { self.capabilities = caps; self }

    /// 可选。默认 `ProtocolVersion::LATEST`。
    pub fn protocol_version(mut self, v: ProtocolVersion) -> Self { self.protocol_version = v; self }

    /// 内部 mpsc channel 容量（command / event 共用此上限），默认 64。
    pub fn channel_capacity(mut self, cap: usize) -> Self { self.channel_capacity = cap.max(1); self }

    /// 注册反向 RPC 处理器。
    pub fn reverse_handler(mut self, h: Arc<dyn ReverseHandler>) -> Self {
        self.reverse_handler = Some(h);
        self
    }

    pub fn initialize_timeout(mut self, d: Duration) -> Self { self.initialize_timeout = d; self }

    // ===== 三个终态 connect 方法。每个消费 self、跑 initialize 握手、返回就绪 Client。 =====

    /// 进程内 spawn 一个 server child；client 拿其 stdin/stdout 跑 JSON-RPC。
    /// Child 由 client 持有 + 在 `shutdown` / drop 时 best-effort kill。
    pub async fn connect_stdio(self, child: Child) -> Result<Client, ClientError> { todo!() }

    /// Unix Domain Socket / Windows Named Pipe。路径由 caller 解析（绝对）。
    pub async fn connect_uds(self, path: PathBuf) -> Result<Client, ClientError> { todo!() }

    /// Remote WebSocket（含 `ws://` loopback + `wss://`）。Phase 3 上线；
    /// Phase 1 草图保留 method 签名占位、内部 `Err(ClientError::Config("phase 3"))`。
    pub async fn connect_remote(self, url: String) -> Result<Client, ClientError> { todo!() }
}

// ========================================================
// Client（同步 + 流式 + 反向 三类 API 入口）
// ========================================================

/// JSON-RPC 客户端。所有 transport（stdio/uds/remote）共此公开类型。
///
/// 三类 API：
/// - **同步**：`request_typed::<T>(method, params).await -> Result<T, ClientError>`
/// - **流式**：`next_event().await -> Option<ClientEvent>`（融合所有 server→client 非应答消息）
/// - **反向**：通过 builder `.reverse_handler(...)` 注册一次；worker 自动 dispatch
///
/// Drop 行为：触发 worker task best-effort shutdown。希望 graceful 时调 `shutdown()`。
pub struct Client {
    inner: ClientInner,
    server_info: Implementation,
    negotiated_version: ProtocolVersion,
    negotiated_server_capabilities: Capabilities,
}

enum ClientInner {
    Stdio(StdioClient),
    Uds(UdsClient),
    Remote(RemoteClient),
}

// 私有 transport 各自一种；公开 API 全部走 `Client` enum dispatch。
struct StdioClient { /* worker_handle / command_tx / event_rx / child */ }
struct UdsClient { /* 同上，stream = UnixStream */ }
struct RemoteClient { /* WebSocketStream */ }

impl Client {
    /// 进入 builder 链。
    pub fn builder() -> ClientBuilder { ClientBuilder::default() }

    /// 协商完成后服务端的 ProtocolVersion（≤ caller 请求版本）。
    pub fn protocol_version(&self) -> ProtocolVersion { self.negotiated_version }

    /// 协商完成后服务端的 capabilities。caller 必须**先**检查此值再
    /// 决定调用哪些 method（避免 -32601 MethodNotFound）。
    pub fn server_capabilities(&self) -> &Capabilities { &self.negotiated_server_capabilities }

    /// 协商完成后服务端的身份信息。
    pub fn server_info(&self) -> &Implementation { &self.server_info }

    // ----- 同步 API -----

    /// 发一条 client→server typed 请求并解码响应。
    ///
    /// `method` 字符串遵循 A2 §5.2 表（裸 `initialize` / `thread/start` / `turn/cancel`，无 v 前缀）。
    /// `params` 必须 `Serialize`；`T` 必须 `DeserializeOwned`。
    pub async fn request_typed<P, T>(&self, method: &str, params: &P) -> Result<T, ClientError>
    where
        P: serde::Serialize + ?Sized,
        T: DeserializeOwned,
    { todo!() }

    /// 同上但返回裸 `serde_json::Value`，给 bridge 类 caller 用。
    pub async fn request_raw<P>(&self, method: &str, params: &P) -> Result<serde_json::Value, ClientError>
    where
        P: serde::Serialize + ?Sized,
    { todo!() }

    /// 发一条 client→server notification（无 response）。
    pub async fn notify<P>(&self, method: &str, params: &P) -> Result<(), ClientError>
    where
        P: serde::Serialize + ?Sized,
    { todo!() }

    // ----- 流式 API -----

    /// 取下一条事件。`None` ⇒ worker 已永久退出（断连后 + Disconnected 事件已 emit 过）。
    ///
    /// 该方法借 `&mut self`：事件 channel 是单 consumer。**多 task 并发请求**
    /// 走 `request_handle()` 拿到的 `RequestHandle`（Clone），事件流由唯一 owner 消费。
    pub async fn next_event(&mut self) -> Option<ClientEvent> { todo!() }

    /// 拿一个可 `Clone` 的请求句柄；用于多 task 并发 `request_typed`。
    /// **不包含事件接收能力**——对齐 codex `AppServerRequestHandle` 设计（lib.rs:469-478）。
    pub fn request_handle(&self) -> RequestHandle { todo!() }

    // ----- 关闭 -----

    /// Graceful shutdown：发关闭 command、等 worker 退出（bounded 5s 兜底 abort）。
    pub async fn shutdown(self) -> Result<(), ClientError> { todo!() }
}

#[derive(Clone)]
pub struct RequestHandle {
    // 内部 mpsc::Sender，Clone 后多 task 共享
}

impl RequestHandle {
    pub async fn request_typed<P, T>(&self, method: &str, params: &P) -> Result<T, ClientError>
    where
        P: serde::Serialize + ?Sized,
        T: DeserializeOwned,
    { todo!() }

    pub async fn notify<P>(&self, method: &str, params: &P) -> Result<(), ClientError>
    where
        P: serde::Serialize + ?Sized,
    { todo!() }
}

impl Drop for Client {
    /// best-effort：发关闭信号但不等。对齐 codex `Drop` 隐式 worker 退出语义
    /// （codex 自己没显式实现 Drop，但 channel 关闭后 worker 自然退出）。
    /// **不 panic、不 abort 进程**（C1-Q4）。
    fn drop(&mut self) { /* todo!() drop sender；worker 退出后写其余 */ }
}
```

> 注：上面 `Notification` / `ServerRequest` / `JsonRpcError` 来自 `zhive-proto`。Phase 1 zhive-proto 已有 `ErrorObject` (B0 之前 `JsonRpcError` 改名，详见 `crates/zhive-proto/src/lib.rs:174-181`)；本草图按 A1/A2 风格使用强类型名。

---

## 3. 与 codex `app-server-client` 的字段对照表

> 列：zhive 字段 / 方法 ↔ codex 同名字段 / 方法 ↔ 备注

### 3.1 顶层 Client 类型

| zhive | codex | 备注 |
|---|---|---|
| `enum ClientInner { Stdio(_), Uds(_), Remote(_) }` 私有 | `pub enum AppServerClient { InProcess(InProcessAppServerClient), Remote(RemoteAppServerClient) }`（lib.rs:480-483） | codex 公开 enum；zhive 把 enum 隐藏在 `Client` 后面，单一公开类型。理由：zhive 三种 transport 都跨进程，不像 codex 还有 in-process 优化路径；caller 不应根据 transport 类型写条件代码。 |
| `Client::builder() -> ClientBuilder` | `InProcessAppServerClient::start(InProcessClientStartArgs)` / `RemoteAppServerClient::connect(RemoteAppServerConnectArgs)`（lib.rs:485, remote.rs:163） | codex 用 args struct + 工厂方法；zhive 用 builder + `.connect_*()` 终态方法。**取舍**：codex args struct 适合大量字段一次性传；zhive 字段数较少（5 个核心 setter）+ builder 支持渐进配置，更人类友好。 |
| `protocol_version() / server_capabilities() / server_info()` | `server_version() -> Option<&str>`（remote.rs:184-186） | codex 只暴露 user-agent 字符串；zhive 暴露 A2 协商三件套（version + capabilities + info），caller 可据此跳过未支持方法。 |
| Drop = best-effort | （codex 无显式 Drop） | codex worker 退出靠 channel close；zhive 同语义但**显式注释**说明不 panic。 |

### 3.2 同步请求 API

| zhive | codex | 备注 |
|---|---|---|
| `request_typed::<P, T>(method: &str, params: &P)` | `request_typed::<T>(request: ClientRequest)`（lib.rs:648） | codex 用 `ClientRequest` enum 把 method + params 绑成一个值；zhive 走「method 字符串 + 任意 serializable params」泛型对 + 把 method 表交给 zhive-proto 常量。理由：D-005 锁定 acp 0.12.1 + rmcp 1.7，三方 schema 共存时枚举集合不稳定；用 `&str` + 强类型 params 在 zhive-proto 侧定义降低 N 个版本枚举的耦合。 |
| `request_raw::<P>(method, params) -> Value` | `request(request: ClientRequest) -> RequestResult` | codex 返回 `Result<JsonRpcResult, JSONRPCErrorError>`；zhive 返回 `Result<Value, ClientError>`，把 server error 抬升到 `ClientError::Server` 让 `?` 链友好。 |
| `notify::<P>(method, params)` | `notify(ClientNotification)`（lib.rs:669） | 同 request 对照；method 用字符串 + params 泛型。 |

### 3.3 流式 + 反向 API

| zhive | codex | 备注 |
|---|---|---|
| `next_event() -> Option<ClientEvent>` 单一事件流 | `next_event() -> Option<InProcessServerEvent>` / `AppServerEvent`（lib.rs:755, 908） | **完全对齐**。融合 Notification + ServerRequest + Lagged + Disconnected 四 case 是 codex 的稳定方案，本调研直接采纳。 |
| `enum ClientEvent { Notification, ServerRequest, Lagged, Disconnected }` | `enum AppServerEvent { Lagged, ServerNotification, ServerRequest, Disconnected }`（lib.rs:131-149） | 字面差异：codex 用 `ServerNotification` 全名；zhive 缩为 `Notification`（client 视角下 server 是唯一发 notification 的方向，无歧义）。 |
| `trait ReverseHandler` + `builder.reverse_handler(Arc<dyn ReverseHandler>)` | **无显式 trait**；codex 在 worker 内对特定 method 硬编码 reject（如 `ChatgptAuthTokensRefresh` 见 lib.rs:556-572），其余通过 `next_event() -> ServerRequest` 由 caller 手动 `resolve_server_request` / `reject_server_request` 应答（lib.rs:691-748） | **重大差异**。zhive 走 trait 而非 caller-driven。理由：zhive `permission/request` 是 hot path（每个 tool_call 一次），用 trait 一次注册比每次都 `next_event` → match → resolve 三步 boilerplate 少。同时**保留** caller-driven 入口：`ClientEvent::ServerRequest` 仍 emit，让 bridge 类高级 caller 旁路接管。详见 C1-Q3。 |
| `resolve_server_request / reject_server_request` 公开 method？ | 是（lib.rs:695-748，被 codex TUI 显式调用） | zhive 草图**不公开**这两个方法 —— 反向应答只走 `ReverseHandler::handle` 一条路径，避免双入口语义混乱。**未决项 C1-N5**：是否给「caller 自己接管事件流」场景留个 escape hatch（建议留，但要文档明确 trait 与 manual 互斥）。 |

### 3.4 Builder / 连接参数

| zhive Builder setter | codex args 字段 | 备注 |
|---|---|---|
| `client_info(Implementation)` | `client_name: String + client_version: String`（lib.rs:359-361；remote.rs:86-87） | codex 拆两个 string；zhive 用 A2 `Implementation { name, title, version }` 三字段对齐 ACP。 |
| `capabilities(Capabilities)` | `experimental_api: bool + opt_out_notification_methods: Vec<String>`（lib.rs:362-365；remote.rs:88-89） | codex 把 capabilities 字段散在 args 顶层；zhive 走 A2 `Capabilities` 强类型集合，含 7 个 flag + 1 个嵌套 `StreamingCapability`。 |
| `protocol_version(ProtocolVersion)` | （codex **不传**版本号）codex `InitializeParams.capabilities` 不含 protocolVersion 字段（v1.rs:43-57） | zhive 走 A2 强协商，version 是 builder 必填 + initialize wire 必填。**重大差异**。 |
| `channel_capacity(usize)` | `channel_capacity: usize`（lib.rs:367；remote.rs:90） | 完全对齐，默认值不同（codex `DEFAULT_IN_PROCESS_CHANNEL_CAPACITY` lib.rs:29 ；zhive 草图取 64）。 |
| `reverse_handler(Arc<dyn ReverseHandler>)` | **无** | zhive 独有。 |
| `initialize_timeout(Duration)` | `INITIALIZE_TIMEOUT = 10s` 硬编码（remote.rs:66） | zhive 暴露给 caller，默认 10s。 |
| `connect_stdio(child: Child)` | （codex InProcess 不需要 child；它把 server 跑在 in-process 任务里）；codex CLI 端的 stdio child spawn 在 `app-server` crate 而非 client lib | **重大差异**。zhive 没有 in-process 同语义（D-002/D-005 锁定 server 是单独 crate，client 与 core 解耦），所以 stdio child 是公开 API 的一部分。 |
| `connect_uds(path)` | `RemoteAppServerEndpoint::UnixSocket { socket_path }`（remote.rs:78-80） | 对齐；zhive 单独 method 而非 endpoint enum 分支，因为不和 ws 共享 setter 模板。 |
| `connect_remote(url)` | `RemoteAppServerEndpoint::WebSocket { websocket_url, auth_token }`（remote.rs:74-77） | 字面对齐；Phase 1 zhive 不实现，仅留 API 占位（D-004 锁定 stdio + uds 为 Phase 1，TCP/TLS Phase 3）。 |

### 3.5 错误类型

| zhive `ClientError` case | codex 对应 | 备注 |
|---|---|---|
| `Transport { method, source: io::Error }` | `TypedRequestError::Transport`（lib.rs:281-284） | 对齐 |
| `Server { method, source: JsonRpcError }` | `TypedRequestError::Server` `JSONRPCErrorError`（lib.rs:285-288） | zhive `JsonRpcError` = zhive-proto 已有 `ErrorObject`（lib.rs:174-181） |
| `Deserialize { method, source: serde_json::Error }` | `TypedRequestError::Deserialize`（lib.rs:289-292） | 对齐 |
| `Config(String)` | （codex 在 connect_websocket_endpoint 用 `IoError::new(InvalidInput, ...)`，无专用 case）（remote.rs:686-693） | zhive 分出来便于区分「builder 阶段错」vs「runtime 错」 |
| `Disconnected(String)` | `AppServerEvent::Disconnected { message }`（lib.rs:136）+ worker pending request 全 reject `BrokenPipe`（remote.rs:459-467） | zhive 把 disconnected 同时投递到 event stream **和** request future。详见 C1-Q4。 |

---

## 4. 同步 / 流式 / 反向 三类 API 的拓扑图（ASCII）

```
                                          zhive-client-native::Client
                              (公开 API surface；caller 看不到 Inner/transport 切换)

  ┌──────────────────────────────────────────────────────────────────────────────────┐
  │ Caller (TUI / Bridge / Embedded SDK)                                             │
  └──────────────────────────────────────────────────────────────────────────────────┘
       │   request_typed(...)             next_event()                                  
       │   notify(...)                    ▲                                             
       │   shutdown()                     │                                             
       ▼                                  │                                             
  ┌──────────────────┐ command_tx ┌──────┴────────────────┐ event_rx ┌────────────────┐
  │  Client          │──────────► │   Worker task         │ ────────►│   ClientEvent   │
  │  (公开 facade)    │            │   tokio::select! 三路 │           │   流（mpsc）    │
  │                  │ ◄────────  │                       │           └────────────────┘
  │  + RequestHandle │  oneshot   │  cmd_rx                │
  │  + Drop=         │  per req   │  transport.recv()      │     ▲
  │    best-effort   │            │  reverse_handler.run() │     │ 反向 RPC 旁路
  │    close         │            └───────────┬─────┬──────┘     │ （仅当 handler
  └──────────────────┘                        │     │            │  未声明此 method）
                                              ▼     ▼            │
                                          stdin/  stdout/        │
                                          (Child/UnixStream/WS)  │
                                                                 │
   同步 request →  command_tx → worker → transport.send → server                 
                      ↑                                                          
                      └ oneshot 回填响应：worker.match_id() → resolve(Ok/Err)      
                                                                                  
   流式 notification ← transport.recv → worker → event_tx → ClientEvent::Notification
                                                                                  
   反向 request ← transport.recv → worker.classify():                              
        │                                                                          
        ├── method ∈ handler.methods() → spawn handler.handle(method, params).await
        │       └── on Ok(v)   → transport.send(Response{id, result: v})           
        │       └── on Err(e)  → transport.send(Error{id, error: e})               
        │       └── 同时 emit ClientEvent::ServerRequest 让旁观者也能看到（可选）
        │                                                                          
        └── method ∉ handler.methods() → transport.send(Error{id, MethodNotFound}) 
                                                + emit ClientEvent::ServerRequest  

   Disconnected:                                                                   
        transport.recv() = Eof / Close / Err →                                     
            worker.broadcast_disconnect(message)                                   
              ├── for each pending request: oneshot.send(Err(Disconnected))        
              ├── event_tx.send(Lagged 累积) [尽力]                                  
              ├── event_tx.send(Disconnected { message })                          
              └── worker task 退出（不 panic）                                       
                                                                                  
   Drop:                                                                           
        drop(command_tx) → worker.select_recv = None → graceful_close → 退出       
        （不等待 = best-effort；caller 想 deterministic 关闭走 client.shutdown().await） 
                                                                                  
                                                                                  
                                ┌──────────────────────────────────┐                
   Server side（对照参考，不在    │  zhive-core::Engine + Server      │                
   本 deliverable 范围内）        │  （C2/C3/C4 处理）                 │                
                                └──────────────────────────────────┘                
```

**关键拓扑性质**：
1. **command 路径** 单 mpsc::Sender（caller 多 task 通过 `RequestHandle::clone()` 共享）→ 单 worker → transport。worker 内部用 HashMap 跟踪 in-flight request id。
2. **event 路径** 单 mpsc::Receiver（**单 consumer**：caller 拿 `&mut Client` 才能 `next_event`）。lossless 通知通过 codex 同款 `event_requires_delivery` 策略阻塞 send；best-effort 通知走 try_send（lib.rs:175-186）。
3. **反向路径** transport → worker → reverse_handler.handle → transport.send（response/error）。在 cancel 时 worker 优先发 `Cancelled` outcome 再放弃 future（A3 §6.2 决策）。

---

## 5. 关键问题逐条作答（每条 ≤ 8 行）

### Q1：三种 connect 是否共一个 builder？

**共一个 builder**。`ClientBuilder` 携带 transport-无关的配置（client_info / capabilities / version / channel_capacity / reverse_handler / initialize_timeout），三个 `connect_*` method 是**终态消费方法**，每个接 transport-specific 入参（`Child` / `PathBuf` / `String url`）。理由：(a) caller 体验 fluent（先配 caps 再选 transport）；(b) 避免 codex 那种 `InProcessClientStartArgs` 与 `RemoteAppServerConnectArgs` 两套字段错位（codex lib.rs:330-368 vs remote.rs:84-91 字段名/默认值有微妙差异，长期维护负担）；(c) 三 transport 共享 initialize 握手逻辑，单 builder 单 initialize timeout 字段。**不选 B（每个 connect 各自的 builder）**：会重复 6+ setter。

### Q2：同步 vs 流式 API 分离还是融合？

**分离**。`request_typed` 走 oneshot per-request；`next_event` 走 mpsc 单 stream（融合 notification + reverse request + lagged + disconnected）。**不选 B（统一 `subscribe(method) -> Stream<Notification>`）**：LSP 走那条路是因为它没有 reverse-request；zhive 三类 server→client 消息形态（notification / reverse request / disconnected）必须有统一通道（caller 不能开 N 个 stream 还要并发处理 reverse RPC）。codex 也是分离设计（`request*` vs `next_event`，lib.rs:620 / 755），调研直接采纳。优势：caller 一个 `tokio::select!` 就能同时处理用户输入 + 服务端事件。

### Q3：Reverse-request 由谁处理？trait 还是预注册 closure？

**`trait ReverseHandler` + builder 注册一次**。具体形态见 §2.2 草图。理由：(a) 闭包要装箱 + 单 method 单闭包难复用上下文（permission UI 弹窗的 state 通常在一个 struct 上）；(b) trait 可声明 `methods()` 让 worker fast-path 拒绝未知方法（避免每个未知 method 都 alloc handle future）；(c) caller 也可同时通过 `ClientEvent::ServerRequest` 旁观（高级 bridge 用例），但 worker 已自动应答 —— 双入口语义文档需明确 trait 优先（**未决项 C1-N5**）。**不选 B（预注册 closure map: HashMap<&str, Box<dyn Fn>>）**：闭包要 `Send + Sync + 'static`，状态难管理。

### Q4：Drop 行为：连接异常时 panic 还是 error stream？

**Error stream 双投递，永不 panic**。worker 检测到 transport eof / err 时：(i) 所有 in-flight `request_typed` 的 oneshot 都 resolve 成 `Err(ClientError::Disconnected)`；(ii) 同时 emit `ClientEvent::Disconnected { message }` 到事件流；(iii) worker task 退出（不 panic、不 abort）。`Client::drop` 仅 drop sender → worker 检测到 channel close → 自然退出 best-effort 关闭 transport。**Caller 想 deterministic 关闭**：调用 `client.shutdown().await`（带 5s SHUTDOWN_TIMEOUT 兜底 abort，对齐 codex lib.rs:122 / 763-795）。**不选 B（panic on transport error）**：zhive 是库 crate，panic 会污染 caller 的 runtime；codex 也走 error 路径（remote.rs:402-467 整个分支处理 disconnect）。

---

## 6. 与 A1 / A2 / A3 的对接点表

### 6.1 Client expose 哪些 A1 类型

| client method / 字段 | 涉及 A1 类型 | 用法 |
|---|---|---|
| `request_typed::<_, ThreadStartResponse>("thread/start", &params)` | `Thread` / `ThreadId` / `ThreadSource` | server 响应反序列化 |
| `request_typed::<_, Turn>("turn/start", &params)` | `Turn` / `TurnId` / `TurnStatus` | 同上 |
| `ClientEvent::Notification(...)` 携带 | `TurnStartedNotification { thread_id, turn }` / `TurnCompletedNotification { thread_id, turn }` | wire method = `events/turn_started` / `events/turn_completed`（A1 §2.3 决策） |
| `ClientEvent::Notification(...)` 携带 | `SessionAbortedNotification { cleared_steer, cleared_follow_up, next_turn_retained_count }` | wire method = `events/session_aborted`（A3 §8） |

### 6.2 初始化用什么 A2 字段

| ClientBuilder setter | A2 字段 | 协商行为 |
|---|---|---|
| `.client_info(Implementation { name, title, version })` | A2 §2 `Implementation`（agent.rs:197-220 对齐） | wire 必填；server 拒绝空 name |
| `.capabilities(Capabilities { hooks, subagents, streaming, cancellation, permission, extension, experimental_api, ... })` | A2 §3 `Capabilities` 7 flag + 1 嵌套 `StreamingCapability` | client 声明能力；server 响应里回 `server_capabilities` 字段，caller 拿来过滤可用 method |
| `.protocol_version(ProtocolVersion::V1)` | A2 §2 `ProtocolVersion(u16)` | server `min(server_latest, request)`；超出 → `ErrorObject.code = -32001 ProtocolVersionUnsupported`（A2 §6.Q1） |
| `.initialize_timeout(Duration)` | A2 wire method `"initialize"` + 收尾 `"initialized"` notification（A2 §2 TODO-A2.3 决策保留） | 10s 默认；超时 ⇒ `ClientError::Transport { method: "initialize", ... }` |

### 6.3 Reverse 注册哪些 A3 method

| `ReverseHandler::methods()` 项 | A3 来源 | params 类型 | 返回值 |
|---|---|---|---|
| `"session/request_permission"` | A3 §2 `PermissionDecision` 四态 + ACP `RequestPermissionRequest`（client.rs:555-756） | `{ threadId, turnId, itemId, toolName, toolInput, scope }` | `{ decision: "allow" \| "deny" \| "ask" \| "defer", reason?, updated_input? }`（A3 §5 `HookSpecificOutput::PreToolUse`） |
| `"hook/run"` | A4（依赖 A3） | `{ hook_event_name, ... }` | `HookOutput`（A3 §5） |
| `"session/request_user_input"` | codex `ToolRequestUserInputParams` 移植（lib.rs:957-958） | `{ thread_id, turn_id, item_id, questions: [...] }` | `{ answers: [...] }` |

**Cancel 与 reverse-RPC 交互**（A3 §6.3 验收）：worker 在收到 `session/cancel` notification 后 / 触发 abort_token 时，**先**遍历 in-flight reverse request 的 `pendingReverse: HashMap<RequestId, ...>`，逐个 `transport.send(Error{ id, JsonRpcError{ code: -32099, message: "cancelled" }})` 或按 A3 推荐发 `Cancelled` outcome JSON。然后才放弃 `ReverseHandler::handle` future。**worker 不调 `handle` 已发起后的 abort**——`handle` future 自然被 cancellation 触发 drop（caller 的 handler 应自己实现 cooperative cancel）。

---

## 7. 未决项

> TODO(开放项 C1-N1)：`request_typed` 用 `method: &str` + 任意 params 泛型，与 codex `ClientRequest` enum 路线不同。优势是 wire schema 演进无需改 enum 大表；劣势是 caller 容易拼错 method 字符串。**方案 A**：zhive-proto 暴露 `pub const THREAD_START: &str = "thread/start";` 常量表（codex `AGENT_METHOD_NAMES` 同源做法）。**方案 B**：补 `ClientRequest` 强类型 enum，但限定 v1 子集。建议 A（轻量，扩展友好）。B5 / B6 决定。

> TODO(开放项 C1-N2)：`reverse_handler` 是否支持运行时**多 handler 串联**（一个 method 分发到多个 handler 取第一个 non-`None`）？目前草图是单 handler。如果 caller 想分别为 `permission/*` 和 `hook/*` 注册不同 struct，需要 `builder.reverse_handlers(Vec<Arc<dyn _>>)` 或 internal 合并 trait。建议 Phase 1 单 handler，多 handler 需求由 C4 落地决定。

> TODO(开放项 C1-N3)：`channel_capacity` 默认 64 与 codex `DEFAULT_IN_PROCESS_CHANNEL_CAPACITY`（lib.rs:29 实际值需 grep codex-app-server 源码）可能不同；本调研未读 codex 该常量值。一致性 review 在 C2（连接管理）阶段做。

> TODO(开放项 C1-N4)：`connect_stdio(child: Child)` 是否要支持**已存在的 stdin/stdout pipe**（即不接 child，直接接 reader/writer pair）？bridge crate 跑在子进程里时 server 已被父进程 spawn，bridge 拿到的是 fd pair 而非 `Child`。建议补 `connect_pipes(stdin: ChildStdin, stdout: ChildStdout)` 第二个 method。C2 决定。

> TODO(开放项 C1-N5)：双反向入口（`ReverseHandler` 自动 dispatch + `ClientEvent::ServerRequest` 旁观）的语义要明文化。建议：trait 处理是默认路径；caller 可选 `.builder().reverse_handler_passthrough_events(true)` 显式开启旁观 emit。否则未 emit `ServerRequest` event 避免双语义。

> TODO(开放项 C1-N6)：`request_raw` 与 `request_typed` 双入口的存在理由是 bridge-mcp / bridge-acp 转发时不希望解码再编码。但保留两个 API 让用例变多。可只留 `request_typed::<Value>` 让 bridge 类用泛型 `Value` 走同一入口；简化 API。B5/B6 决定。

> TODO(开放项 C1-N7)：`server_capabilities()` 返回 `&Capabilities`；是否需要 helper `client.supports("hook/run") -> bool`，按 method 字符串查 capability 映射？建议有，但表格在 A2 / B6 落地（method ↔ capability 映射表本 deliverable 不深入）。

---

## 8. 验收对照

- [x] 论断带锚点（§1 / §3 / §6 表全部 verbatim 引用 codex 行号 + A1/A2/A3 §节）
- [x] 不动 `crates/` 源码（本 deliverable 所有 Rust 代码块为草图，`todo!()` 占位）
- [x] 不改 `research/99-decisions/`（仅在 §7 未决项中提及，未触动决策文件）
- [x] 不 `git pull`（codex 仅本地读三个文件）
- [x] client 只依赖 proto，无 zhive-core 类型暴露（§2.2 import 列表证）
- [x] 30-45 min 内落盘
