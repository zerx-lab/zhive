---
task: C3
title: 取消处理（zhive-client-native cancel API + 与 B7 server 侧 对接点）
plan: phase1-core-native-research
date: 2026-05-28
status: draft
crate: zhive-client-native（仅依赖 zhive-proto）
depends_on:
  - deliverables/B7-cancel-streaming.md   (三层 CancellationToken / Steer 不撤 in-flight tool_call / pending permission/request abort 时 ACP Cancelled outcome 显式回 / NextTurn 跨 abort 保留)
  - deliverables/C1-client-api.md         (Client / ClientBuilder / ClientEvent 四 case / request_typed oneshot / ReverseHandler)
  - deliverables/C2-reconnect.md          (Disconnected 终态、不自动重连)
  - deliverables/A3-permission-streaming-subagent.md  (session/cancel reverse-RPC 形状 + Cancelled outcome)
references_external:
  - ${ACP_REG}/agent-client-protocol-schema-0.12.0/src/agent.rs                              (`pub struct CancelNotification { session_id: SessionId, meta: Option<Meta> }` + `CancelNotification::new(session_id)`)
  - ${ACP}/src/agent-client-protocol/src/schema/client_to_agent/notifications.rs:1-3        (`impl_jsonrpc_notification!(CancelNotification, "session/cancel")` —— **notification**，无 response)
  - ${ACP}/src/agent-client-protocol/src/schema/client_to_agent/mod.rs                       (cancel 在 client_to_agent/notifications 模块 ⇒ client→agent 单向 notification)
  - ${ACP}/src/agent-client-protocol/src/schema/enum_impls.rs:62                             (`CancelNotification => "session/cancel"` method name 常量映射)
  - ${ACP}/src/agent-client-protocol-cookbook/src/lib.rs:170-188                             (`RequestPermissionOutcome::Cancelled` 在收到 cancel 后 client 端必收的响应形态)
  - ${LSP}/src/service.rs:48-50, 352-357                                                     (`$/cancelRequest` 是 **notification**，`params: { id: RequestId }` —— 与 ACP `session_id` 维度不同)
  - ${LSP}/src/service/pending.rs:14-78                                                      (server 侧 abort 模型：`DashMap<Id, AbortHandle>` + `Pending::cancel(id)` 调 `handle.abort()` + `Pending::cancel_all()`)
  - ${LSP}/src/service/layers.rs:200-210, 265-300                                            (LSP server `Cancellable` layer 把 `$/cancelRequest` 翻译成 `pending.cancel(id)` —— **server 侧**模型；客户端发出方仅是普通 notification)
  - ${CODEX}/app-server-client/src/lib.rs                                                    (codex 全文 grep `cancel` 命中 0 处 —— codex 客户端**未提供** generic cancel API；侧证：取消是协议级 method 而非 client 层 helper)
references_internal:
  - plans/phase1-core-native-research/deliverables/B7-cancel-streaming.md §2-§4              (三层 CancellationToken 树 + pending_approvals lifecycle + drain 送 Cancelled outcome)
  - plans/phase1-core-native-research/deliverables/B7-cancel-streaming.md §3.3               (NextTurn 跨 abort 保留 + Steer 不撤 in-flight tool_call 时序)
  - plans/phase1-core-native-research/deliverables/C1-client-api.md §2.2                     (Client / RequestHandle / ReverseHandler 公开 API surface + Disconnected case)
  - plans/phase1-core-native-research/deliverables/C2-reconnect.md §3-§4                     (Disconnected 终态、in-flight oneshot 一律 `Err(Disconnected)`)
  - plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md §6.3  (pending permission/request 必须用 ACP `Cancelled` outcome 显式回 client)
non-goals:
  - 不写 zhive crate 源码（本 deliverable 所有 Rust 代码块为草图，`todo!()` 占位）
  - 不改 research/99-decisions/
  - 不暴露 zhive-core 类型（client 仅依赖 zhive-proto）
  - 不重新设计 server 侧 cancel 模型（B7 已落定，本文件仅 client 视角对接）
---

> 范围声明：C3 调研产出。client 侧取消 API 形状 + 与 B7 server 侧对接。
> ${ACP} = `~/Desktop/code/github/acp-rust-sdk/`；${ACP_REG} = `~/.cargo/registry/src/index.crates.io-*/`；${LSP} = `~/Desktop/code/github/tower-lsp/`；${CODEX} = `~/Desktop/code/github/codex/codex-rs/`。
> **关键先验**：ACP `session/cancel` 是 **client→agent notification**（无 response；按 session_id 而非 request_id 取消），与 LSP `$/cancelRequest`（按 request_id 取消）维度不同。B7 已固化此语义并要求 server 侧 abort 时 drain pending `permission/request` 用 `Cancelled` outcome 显式回 client。

---

## 1. 参考点清单

| 论断 | 仓库 / 路径 | 行号 |
|---|---|---|
| ACP `CancelNotification` 字段：`session_id: SessionId, meta: Option<Meta>`（**按 session 而非 request** 取消） | `${ACP_REG}/agent-client-protocol-schema-0.12.0/src/agent.rs` | `pub struct CancelNotification` 块 |
| ACP `session/cancel` 是 **notification**（无 response） | `${ACP}/src/agent-client-protocol/src/schema/client_to_agent/notifications.rs` | 1-3 |
| ACP `session/cancel` 方向 = client→agent（位于 `client_to_agent/notifications.rs`） | `${ACP}/src/agent-client-protocol/src/schema/client_to_agent/mod.rs` | — |
| ACP method 名常量：`CancelNotification => "session/cancel"` | `${ACP}/src/agent-client-protocol/src/schema/enum_impls.rs` | 62 |
| LSP `$/cancelRequest` 是 **notification**，`params: { id: RequestId }`（按 **request id** 取消） | `${LSP}/src/service.rs` | 48-50, 352-357 |
| LSP server 侧 abort 模型：`DashMap<Id, AbortHandle>` + `cancel(id) ⇒ handle.abort()` | `${LSP}/src/service/pending.rs` | 14-78 |
| LSP server `Cancellable` middleware：`$/cancelRequest` notification → `pending.cancel(id)` | `${LSP}/src/service/layers.rs` | 200-210, 265-300 |
| codex `app-server-client` **无** generic cancel API（侧证：取消是协议级 method） | `${CODEX}/app-server-client/src/lib.rs` | grep `cancel` 0 命中 |
| B7：`session/cancel` 触发 → `ActiveTurn.cancel.cancel()` + drain `pending_approvals` 送 `Cancelled` outcome + 清 steer/follow_up、保留 next_turn | `deliverables/B7-cancel-streaming.md` §3.3, §4.2 | — |
| B7：pending `permission/request` abort 时**必须**用 ACP `Cancelled` outcome 显式回 client（不能让请求悬挂） | `deliverables/B7-cancel-streaming.md` §3.3, §4.3 | — |
| C1：`Client::request_typed::<_, T>(...)` 走 oneshot；`next_event()` 走单 mpsc 融合 Notification + ServerRequest + Lagged + Disconnected | `deliverables/C1-client-api.md` §2.2, §4 | — |
| C1：`Client::notify(method, params)` 用于 wire-level notification（无 response） | `deliverables/C1-client-api.md` §2.2 | — |
| C1：`ReverseHandler::handle` 在 worker 收到 cancel 时**先**自动应答所有 in-flight 用 `Cancelled` outcome | `deliverables/C1-client-api.md` §2.2, §6.3 | — |
| C2：Disconnected = 终态；caller 自己 drop + rebuild Client | `deliverables/C2-reconnect.md` §2, §4 | — |
| A3：`PermissionDecision` 四态 + ACP `RequestPermissionOutcome::Cancelled` 对接 | `deliverables/A3-permission-streaming-subagent.md` §2, §6.3 | — |

---

## 2. 取消 API 形状（zhive-client-native 公开草图）

### 2.1 设计 invariant

1. **取消粒度对齐 ACP `session/cancel` = per-session（turn）**，**不是** per-request。`session_id` 在 zhive 语境 ≈ thread_id 上的当前 turn 的 root（B7 §2.1 ThreadHandle.cancel）。这是 ACP 的**强约束**：调一次 `session/cancel` 撤的是「这个 session 当前 in-flight 的整个 turn + 所有 pending reverse-RPC」，不是单个 `request_typed` 的句柄。
2. **不在 `request_typed` 返回值上挂 `CancellationToken`**：因为 zhive 的 cancel 是 session 维度（撤的是 turn 不是单 request），把 token 挂到单 request 的句柄上会**误导 caller** 以为 `token.cancel()` 只撤这一条 request。
3. **提供两个层级的 API**：
   - **底层**：`client.notify("session/cancel", params).await` —— 走 C1 通用 notify，caller 自己组 params。**这一条已被 C1 §2.2 覆盖**，本 deliverable 不重复定义。
   - **顶层 helper**：`client.cancel_session(session_id).await` —— 包薄一层 typed param + method 字符串常量，避免 caller 拼错 method 名。
4. **`Drop` in-flight `request_typed` future 不自动发 cancel**：原因 §4 详述。要 cancel turn，caller **必须显式**调 `cancel_session` 或 `notify("session/cancel", ...)`。
5. **`session/aborted` notification 回收**：cancel 后 server 会发 `session/aborted { cleared_steer, cleared_follow_up, next_turn_retained_count }`（B7 §3.3）—— caller 通过 `next_event() → ClientEvent::Notification` 接收。**不**提供 sync `cancel().await -> AbortedNotification` API（避免双通道：notification 走 event 流，应答走 oneshot 会让 caller 两种取数路径都得写）。

### 2.2 Rust 草图（zhive-client-native 公开 API surface 增量）

```rust
//! C3 增量草图：append 到 C1 §2.2 的 `impl Client { ... }`。
//! 不引入新 type，仅一个 helper method。

use zhive_proto::{SessionId, Notification};  // zhive-proto 既有类型（A1 / A3）

impl Client {
    /// 发 ACP `session/cancel` notification，请求 server 取消此 session 当前 turn。
    ///
    /// **语义**（B7 §3.3）：
    /// - 触发 server 侧 `ActiveTurn.cancel.cancel()` —— provider stream / tool 执行 / hook 全部 cancel
    /// - **清空** steer / follow_up 队列（内容通过 `session/aborted.cleared_*` 字段返回 client）
    /// - **保留** next_turn 队列（用于下次 `session/prompt` 注入）
    /// - **drain pending `permission/request`** —— 每个 in-flight reverse-RPC 收到
    ///   `RequestPermissionResponse { outcome: Cancelled }` 应答（ACP 0.12 硬约束，B7 §4）
    /// - server 在 abort 完成后发 `session/aborted` notification（caller 通过 `next_event()` 接收）
    ///
    /// **本调用立即返回**（notification 无 response）。caller 想等 abort 完成 ⇒
    /// 自己 loop `next_event()` 直到看到 `ClientEvent::Notification(SessionAborted { .. })`。
    ///
    /// **不撤销 in-flight tool_call**（Pi 模式，B7 §3.1）：已 fire 的 syscall / HTTP / 子进程
    /// 会跑完；cancel 仅停"再继续做更多工作"，不回滚已做的工作。
    ///
    /// # Errors
    ///
    /// - `ClientError::Transport` —— transport 写失败
    /// - `ClientError::Disconnected` —— 已断连（worker 已退）
    /// - **不**返回 `ClientError::Server`：notification 无 response，server 错误只能从
    ///   后续 `session/aborted` 或别的事件里推断
    pub async fn cancel_session(&self, session_id: &SessionId) -> Result<(), ClientError> {
        // 内部：
        //   self.notify("session/cancel", &CancelParams { session_id: session_id.clone(), meta: None }).await
        // CancelParams 由 zhive-proto 提供（A3 衍生 + ACP 0.12 schema 对齐）
        todo!()
    }
}

impl RequestHandle {
    /// 同 `Client::cancel_session`，可在 clone 的 handle 上调（用于「另一个 task 触发 cancel」场景）。
    pub async fn cancel_session(&self, session_id: &SessionId) -> Result<(), ClientError> {
        todo!()
    }
}
```

### 2.3 *不*提供的 API（带理由）

| API | 状态 | 不提供理由 |
|---|---|---|
| `request_typed::<...>(...).await -> (Result<T>, CancellationToken)` | **不提供** | cancel 是 session 维度，单 request 句柄上挂 token 会误导（详 §4） |
| `client.cancel(request_id)` 按 request id 取消单 request | **不提供** | ACP `session/cancel` 不接受 request id 维度（schema 字段仅 `session_id`）。要按 request id 走只能自造 method，偏离协议 |
| `client.cancel_all_sessions()` 取消所有 session | **不提供** | ACP 无此 method；caller 想全撤 ⇒ 自己遍历 thread_id 列表分别调 `cancel_session` |
| `cancel_session(session_id).await -> SessionAbortedNotification` 等 abort 完成 | **不提供** | abort notification 走 event 流（C1 §4 单一事件通道）；helper 等结果会引双通道、复杂化 backpressure；caller 想等就在 event loop 里 match |
| `Drop` impl on `RequestFuture` auto-cancel | **不提供** | drop 单 request future 不应触发 server-side abort —— **N×M 副作用陷阱**（详 §4） |

---

## 3. 与 server 侧（B7）的对接点

### 3.1 wire 字段（client → server）

| 字段 | 类型 | 来源 | 必填？ |
|---|---|---|---|
| `method` | `"session/cancel"` 字符串 | ACP `enum_impls.rs:62` verbatim | 是（JSON-RPC notification） |
| `params.session_id` | `SessionId`（zhive-proto；ACP 0.12 schema 对齐） | ACP `CancelNotification.session_id` | 是 |
| `params._meta` | `Option<Meta>`（key-value bag） | ACP `CancelNotification.meta` | 否（默认 None） |
| **JSON-RPC `id`** | — | — | **省略**（notification 不带 id） |

**wire 形态**（JSON）：

```json
{ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "thread-abc-turn-xyz" } }
```

> ACP `SessionId` 在 zhive 语境的映射由 A1 / B4 决定。当前共识（B1 §2.1 + A1 §6）：`SessionId = ThreadId` ⇒ 取消"thread 的当前活动 turn"；若一个 thread 同时只能有 ≤1 active turn，则 session_id 唯一映射当前 turn。

### 3.2 server 侧响应路径（client 视角）

收到 cancel notification 后 server 侧动作（B7 §3.3 verbatim 时序）：

```
client                                  server (B7)
  │ notify("session/cancel", {sid})       │
  ├──────────────────────────────────────►│
  │                                       │ 1. cleared_steer = steer; steer = []
  │                                       │ 2. cleared_follow_up = follow_up; follow_up = []
  │                                       │ 3. next_turn 保持不动
  │                                       │
  │                                       │ 4. ActiveTurn.cancel.cancel()
  │                                       │    └─ provider stream / tool exec / hook 全 cancel
  │                                       │
  │                                       │ 5. for (req_id, sender) in pending_approvals.drain():
  │                                       │       sender.send(RequestPermissionResponse {
  │                                       │           outcome: Cancelled,
  │                                       │           meta: None,
  │                                       │       })  ── ACP 0.12 硬约束
  │                                       │    每个 sender 出站后 worker 写 wire 应答给 client：
  │                                       │
  │  反向 RPC 应答回收（N 条 in-flight）   │
  │  ◄────────────────────────────────────│ {jsonrpc:"2.0", id:<req_A>, result:{outcome:"cancelled"}}
  │  ◄────────────────────────────────────│ {jsonrpc:"2.0", id:<req_B>, result:{outcome:"cancelled"}}
  │  ...                                  │
  │                                       │ 6. emit session/aborted notification
  │  ◄────────────────────────────────────│ {jsonrpc:"2.0", method:"session/aborted",
  │                                       │  params: { cleared_steer, cleared_follow_up,
  │                                       │            next_turn_retained_count }}
  │                                       │
  │  (此后所有原 turn 内的 streaming      │
  │   notification 停止；新 turn 可起)     │
```

### 3.3 client 侧 worker 处理路径

worker 收到反向 RPC 应答 + `session/aborted` notification 时的动作（C1 §4 + 本文件）：

| 入站消息 | worker 动作 | 出站到 caller |
|---|---|---|
| `result: {outcome:"cancelled"}` for in-flight `permission/request` reverse-RPC | **不**调 `ReverseHandler::handle` 后处理；该 reverse-RPC 在 worker 内的 pending_reverse Map 上 `take + drop`（handle future 自然被 drop） | 若 `ReverseHandler::handle` 已 spawn 但未 await 完成 ⇒ tokio cooperative cancel 会让 future drop（**前提**：handle 内部用 `tokio::select!` + 自己的 cancel 信号，否则跑完） |
| `session/aborted` notification | 走 C1 §2.2 通用 notification 路径 | `ClientEvent::Notification(SessionAbortedNotification { ... })` 投递到 event 流 |
| 原 turn 的 `turn/completed` / `item/completed` 等 streaming notification | 已在 cancel 前可能 in-flight；worker 不特殊处理（按到达顺序 emit） | caller 在 event 流中可能看到 cancel **之后**还有少量 streaming 事件（race 窗口），按 `session/aborted` 为终止边界处理 |

### 3.4 caller 端典型代码模式（取消 + 等 abort 完成）

```rust
// 草图：caller 在 TUI / bridge 里 cancel 一个 session 并等 server abort 完成
let cancel_handle = client.request_handle();  // Clone 一份给 cancel task
let session_id = current_session_id.clone();

// 触发 cancel（一行）
cancel_handle.cancel_session(&session_id).await?;

// 同时 event loop 里 match SessionAborted
while let Some(event) = client.next_event().await {
    match event {
        ClientEvent::Notification(n) if n.method() == "session/aborted" => {
            let aborted: SessionAbortedNotification = n.deserialize()?;
            println!("aborted; cleared_steer={}, cleared_follow_up={}, next_turn_retained={}",
                aborted.cleared_steer.len(),
                aborted.cleared_follow_up.len(),
                aborted.next_turn_retained_count,
            );
            break;
        }
        ClientEvent::Disconnected { message } => return Err(message.into()),
        _ => continue,
    }
}
```

### 3.5 pending request 在 cancel 期间的命运（client 视角）

| in-flight 状态 | cancel 发出后命运 | 锚点 |
|---|---|---|
| client→server `request_typed("turn/start", ...)` 还在等 response | **不受 `cancel_session` 影响**：cancel 是 turn 维度，turn/start 本身是握手；若 server 在收到 cancel 后才回响应，caller 拿到的可能是 `Ok(turn)` 也可能是 server 主动发的某种错误（schema 待 B7-3 决） | B7 §3.3, 本文 §3.2 |
| server→client `permission/request` 反向请求；client 端 `ReverseHandler::handle` 跑中 | server 主动用 `Cancelled outcome` 应答这条反向 RPC（B7 §4）；worker 收到应答后把 future drop；handle 内部如果用 cooperative cancel 模式（`select!{ cancel.cancelled() => ... }`）会立即退出，否则跑完丢弃返回值 | B7 §4, C1 §6.3 |
| 原 turn 的 streaming notification（`turn/event`, `item/streamed`） | 在 abort 处理过程中可能继续 emit 几条 ⇒ caller 在 event 流里看到 race；按 `session/aborted` 为终止边界丢弃后续 | B7 §3.3 |
| `cancel_session` 自身 | notification 无 response，单向 fire-and-forget；**永远不会** stuck 在 await 上 | 本文 §3.1 |

---

## 4. `Drop` in-flight `Future` 时的行为表

> 核心问题：caller 在 `request_typed(...).await` 上 `tokio::select!{ _ = timeout => break, r = req => r }` —— req future 被 drop。此时 client worker / server 各自怎么反应？

### 4.1 行为对照表

| 操作 | client worker 行为 | server 行为 | 是否触发 wire-level cancel？ |
|---|---|---|---|
| `request_typed` future 被 caller `drop` | **leak**：worker 已把请求写到 transport；oneshot::Sender 仍在 worker 的 pending_requests Map；server 响应到达时 worker 试图 `oneshot.send(...)` 失败（Receiver 已 drop）⇒ worker 丢弃响应、移除 pending entry | server 不知道 caller 已不要响应；**server 继续按部就班处理**这条 request | **否** |
| `cancel_session(sid)` 显式调用 | worker 写 `session/cancel` notification 到 transport | server 走 §3.2 路径（abort + drain pending + emit aborted） | **是**（显式触发） |
| `next_event` future 被 drop（`tokio::select!` 不选这个 arm） | mpsc::Receiver 借 `&mut self`，drop future 仅释放借用，Receiver 本身仍在 `Client` 上；下次 `next_event` 仍能继续接 | 不影响 | 否 |
| `Client::drop`（整个客户端释放） | command_tx drop → worker select_recv = None → 走 C2 §2 `Closing → Closed`；transport drop（child kill / stream close） | server 端读 EOF；按 B7 §4.2 表：**pending_approvals oneshot 全 drop**（RAII），wire 上**不发**任何 Cancelled outcome（C2-N4 已记） | 否（transport 死，无法发） |
| `RequestHandle::drop`（仅 drop handle） | 只是 drop 一份 mpsc::Sender clone；worker 仍活 | 不影响 | 否 |

### 4.2 选型决策：**Drop 不 auto-cancel**

**采纳**：drop in-flight request future 不触发 `session/cancel`。

**理由**：

1. **N×M 副作用陷阱**：一个 session 内可能并发 N 条 `request_typed`；drop 其中 1 条不应取消整个 session 的所有 work。若 auto-cancel session 维度 → drop 任何一条都炸 session，不可接受。
2. **粒度错配**：ACP `session/cancel` 是 session 维度；单 request future drop 不能精准映射"取消这一条而不影响其他"——zhive 又**不**自定义 per-request cancel（§2.3）。要么不动要么炸整 session，前者不偏离 ACP 语义。
3. **codex 同模式**：codex `app-server-client` 没有任何 cancel-on-drop 逻辑（grep `cancel` 0 命中）；drop 后 worker 把响应丢弃即可。zhive 直接采纳。
4. **caller 显式优于隐式**：caller 想取消 ⇒ 调 `client.cancel_session(sid)`，wire 形态可观测可调试；隐式 cancel-on-drop 会让 server 在不期望的时机收到 cancel（典型坑：caller `timeout(3s, req).await` 触发 drop → server 收到 cancel，但其实 caller 只是想跑下一轮 retry）。
5. **leaked response 成本可控**：worker 收到响应后 `oneshot.send` 失败立即丢弃响应 + 移除 pending entry —— 最多浪费一次 server 计算，**不会**内存泄漏（pending Map 在响应到达时清理）。

**反方案 B（Drop auto-cancel）的代价**：

- N×M 误炸：见 1
- wire 流量×N：每次 caller `tokio::select!` timeout 都触发一次 `session/cancel`
- caller 期望违反：caller 用 `timeout` 想"放弃这一轮 wait"通常不想 server abort 整个 session
- 复杂度上升：需要在 oneshot::Receiver drop 钩子里发 wire 消息（异步上下文 + `&mut transport` 需求）—— 工程上几乎一定要引 spawn 一个 cleanup task，破坏 RAII 干净

### 4.3 worker 端 pending Map cleanup（与 B7 §4 server 侧对偶）

worker 内的 `pending_requests: HashMap<RequestId, oneshot::Sender<...>>`（C1 §4 拓扑图）在以下时点清理：

| 触发 | 操作 | 锚点 |
|---|---|---|
| 正常响应到达 | `remove(id) + send(Ok(response))` | C1 §4 拓扑图 |
| `oneshot::Sender.send` 失败（caller 已 drop receiver） | **忽略 + remove(id)**；不报错，不重发 | 本文 §4.1 第 1 行 |
| `cancel_session` 触发 | **worker 不动 pending_requests**；session cancel 不影响 client→server 已发出的请求（粒度错配） | 本文 §3.5 |
| 断连（`Closing`） | 遍历 `drain` + `Err(Disconnected)`（C2 §3） | C2 §3 |

---

## 5. 关键问题逐条作答

### Q1：`Client::request()` 返回值是否带 `CancellationToken`？

**不带。** `request_typed(...).await -> Result<T, ClientError>` 平铺返回，**不挂** `CancellationToken`。理由：(a) zhive 取消粒度 = session（ACP `session/cancel` 仅按 `session_id`，§3.1），与单 request 维度错配；(b) 把 token 挂到单 request 句柄上会让 caller 误以为 `token.cancel()` 只撤这一条 request，实际触发的是整 session abort —— 这是**语义陷阱**。caller 想取消 ⇒ 显式调 `client.cancel_session(sid)`（§2.2）或底层 `client.notify("session/cancel", ...)`。codex `app-server-client` 同样不挂 token（grep `cancel` 0 命中）。

### Q2：`client.cancel(turn_id)` 是单独 request 还是 notification？

**Notification。** zhive `cancel_session(session_id)` 内部走 `client.notify("session/cancel", ...)`，**无 response**。锚点：(a) ACP `CancelNotification` 由 `impl_jsonrpc_notification!` 而非 `impl_jsonrpc_request!`（${ACP}/...notifications.rs:1-3）；(b) ACP 把 cancel 放在 `client_to_agent/notifications.rs` 而非 `requests.rs`；(c) LSP `$/cancelRequest` 同样是 notification（${LSP}/src/service.rs:48-50）。两个参考协议一致：cancel 都是 **fire-and-forget notification**，server 处理后通过**独立**的 `session/aborted` notification 反馈结果。zhive 直接采纳。

### Q3：Drop in-flight `Future` 时是否自动发 cancel？

**不发。** drop `request_typed` future 仅释放 caller 侧的 `oneshot::Receiver`；worker 仍持 `oneshot::Sender`，server 响应到达时 worker `send` 失败 → 丢弃响应 + 移除 pending entry。**Wire 上不发任何 cancel**。理由：(a) 粒度错配 —— session 维度 cancel 撤的是整 session，drop 单 request 触发会炸整 session（N×M 误炸）；(b) caller `timeout(3s, req).await` 典型场景**不想** server abort，仅想自己换条思路重试；(c) codex 同模式（无 cancel-on-drop）；(d) leak 成本可控（响应到达时一次性清理，无内存泄漏）。caller 要 cancel ⇒ **必须显式**调 `cancel_session` —— 显式优于隐式。

---

## 6. 与 C1 / C2 / A3 / B7 衔接核对

| 项 | 来源 deliverable | C3 对接位点 |
|---|---|---|
| `Client::notify(method, params)` 通用 notification 入口 | C1 §2.2 | `cancel_session` 内部调用此 method |
| `ClientError::{Transport, Disconnected}` 两态 | C1 §3.5 | `cancel_session` 的 Err 仅这两种（无 Server 态：notification 无 response） |
| `ClientEvent::Notification(...)` 单一事件通道 | C1 §2.2, §4 | `session/aborted` 经此投递 |
| `ClientEvent::Disconnected { message }` 终态 | C1 §2.2 | 断连后 `cancel_session` 立即 `Err(Disconnected)` |
| Disconnected = 终态，不自动重连 | C2 §4 | cancel 期间 / 之后断连 ⇒ caller drop + rebuild Client；旧 `session_id` 在新 Client 上无效 |
| `RequestHandle` 多 task clone | C1 §2.2, §3.4 | 提供 `RequestHandle::cancel_session` 对偶，多 task 都能触发 cancel |
| ACP `Cancelled` outcome 必须显式回 | A3 §6.3, B7 §3.3 | client 端 worker 在收到 `result:{outcome:"cancelled"}` for in-flight `permission/request` 时 **不调** `ReverseHandler::handle` 后处理（B7 §3.3 server 主动发应答给 client） |
| Steer 不撤当前 tool_call | B7 §3.1 | client 调用 `cancel_session` 时**整个 turn 都被撤**；steer 是另一条路径（不引发 cancel） |
| NextTurn 跨 abort 保留 | B7 §3.3 | client 在 cancel 后下次 `session/prompt` 时 server 自动 splice nextTurn 到 user message 前；client 无需特殊处理 |
| pending `permission/request` 必发 Cancelled outcome | B7 §4.2 | client worker 把 in-flight reverse-RPC 的 future 自然 drop；caller 注册的 `ReverseHandler` 若用 cooperative cancel 可立即退出，否则跑完丢弃 |
| `SessionId` 类型 | A1（待落） / ACP schema 0.12 verbatim | 由 zhive-proto 暴露；C3 在 `cancel_session(sid: &SessionId)` 直接消费 |

---

## 7. 未决项

> TODO(开放项 C3-N1)：`cancel_session(sid)` 调用时若 server 尚未起此 session（例如 caller 在 `request_typed("thread/start")` 还未返回前就 cancel）—— ACP schema 未定义此场景。倾向 server 静默忽略（cancel 是 fire-and-forget，**找不到 sid 不报错**），但 B7 没明确写。建议 B7 / B4 落地时补 server 侧 "unknown sid → log warn + ignore" 语义；C3 草图按"never errors on wire" 假设。

> TODO(开放项 C3-N2)：是否在 `Client` 上提供 `cancel_session_and_wait(sid).await -> SessionAbortedNotification` 同步 helper（包薄一层：发 cancel + 在 event 流里 match aborted）？倾向**否**（避免双通道：event 流 + 此 helper 都能拿到 aborted；caller 不知道用哪个）。但 IDE / bridge 类 caller 强烈需要"等 abort 完成"语义时再考虑。暂留 caller 自己写 `next_event` loop（§3.4 示例）。

> TODO(开放项 C3-N3)：`Client::shutdown()`（C1 §2.2 graceful path）调用时是否要先发 `session/cancel` 给所有已知 in-flight session？目前草图：shutdown 仅 drop transport（C2 §3 表）；server 端通过 transport EOF 推断 client 走人，**不收**任何 wire cancel。这与"cancel 时 server 必须 drain pending_approvals 发 Cancelled outcome"语义有别（B7 §4.2 表"shutdown"行：RAII drop，不发 wire 回包）—— 一致。但 IDE UX 上若 server 没收到 cancel notification 就走 EOF，可能 log 一堆"client died unexpectedly" —— B5 / B6 决定。

> TODO(开放项 C3-N4)：`cancel_session` 是否接受批量 session_id（`cancel_sessions(sids: &[SessionId])`）？ACP schema 单条 `session_id`；zhive 想加批量必须循环。Phase 1 不做，保持 ACP wire 1:1。caller 想批量 ⇒ 自己 `for sid in ... { client.cancel_session(sid).await?; }`。

> TODO(开放项 C3-N5)：client 视角下"已发出的 `request_typed` 在 cancel 期间到底有没有响应"无 wire 保证。例如 caller 同时发 `turn/start` + `cancel_session` 在 server 收到顺序未知。建议 server 侧（B7）补"cancel notification 到达时 currently-handling request 的处理策略"决策：(a) 让正在跑的 handler 跑完正常回响应；(b) 让 handler 也走 cancel 路径回 -32099 cancelled。当前 B7 倾向 (a)（cancel 仅撤 turn-level concept，request handler 本身是 RPC 层不受影响）。

> TODO(开放项 C3-N6)：`Client::drop` 时是否要 best-effort 发一次"广播 cancel 所有 known sessions"？倾向**否**（同 C3-N3 理由）；shutdown 是 process-level，server 通过 transport EOF 自然清理。但需在 user-facing 文档明示"drop Client 不等于发 session/cancel"。

---

## 8. 验收对照

- [x] 论断带锚点（§1 / §3 / §4 全部 verbatim 引用 ACP / LSP / codex / B7 / C1 / C2 / A3 行号 + §节）
- [x] 不动 `crates/` 源码（本 deliverable 所有 Rust 代码块为草图，`todo!()` 占位）
- [x] 不改 `research/99-decisions/`（§7 未决项均在本文件内）
- [x] 不 `git pull`（ACP 读 `notifications.rs` + `enum_impls.rs` + cargo registry schema 0.12.0；LSP 读 `service.rs` + `service/pending.rs` + `service/layers.rs`；codex grep `cancel`）
- [x] client 仅依赖 zhive-proto，无 zhive-core 类型暴露
- [x] 25-40 min 内落盘
