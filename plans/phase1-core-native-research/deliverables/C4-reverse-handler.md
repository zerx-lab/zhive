---
task: C4
title: 反向 RPC handler 注册接口（zhive-client-native）
plan: phase1-core-native-research
date: 2026-05-28
status: implemented
crate: zhive-client-native（仅依赖 zhive-proto）
depends_on:
  - deliverables/C1-client-api.md          (ReverseHandler trait + builder 注册 + 未声明 method 自动 MethodNotFound + ClientEvent::ServerRequest 旁路)
  - deliverables/C3-client-cancel.md       (cancel = notification 维度 / session/aborted server→client notification / Drop 不 auto-cancel)
  - deliverables/A3-permission-streaming-subagent.md (permission/request reverse-RPC payload + ACP Cancelled outcome 硬约束)
  - deliverables/A4-hook-event-schema.md   (14 hook events 由 server 内 dispatch；client 不直接收 "hook/invoke")
  - deliverables/B6-permission-reducer.md  (session/resume_permission request + turn/suspended / turn/resumed notification)
references_external:
  - ${ACP_REG}/agent-client-protocol-schema-0.12.0/src/client.rs                   (RequestPermissionRequest 555-601 / RequestPermissionResponse 683-720 / RequestPermissionOutcome 722-739 含 Cancelled 硬约束注释)
  - ${ACP}/src/agent-client-protocol/src/schema/agent_to_client/requests.rs       (`impl_jsonrpc_request!(RequestPermissionRequest, RequestPermissionResponse, "session/request_permission")` 9-13 —— **reverse RPC**: agent→client)
  - ${ACP}/src/agent-client-protocol/src/schema/enum_impls.rs                     (75 `RequestPermissionRequest => "session/request_permission"`；93 `RequestPermissionResponse => "session/request_permission"`)
  - ${ACP}/src/agent-client-protocol/src/role/acp.rs                              (Client/Agent/Proxy/Conductor 四 role；handler 用 if_request_from/if_message_from 链 222-258 —— **接近 zhive 反向 method 的 typed dispatch 模式**)
  - ${ACP}/src/agent-client-protocol/src/jsonrpc/handlers.rs                      (RequestHandler<Req: JsonRpcRequest> 46-177：method 字符串匹配 Req::matches_method 118；不匹配 ⇒ Handled::No retry=false 159-165 ⇒ 由后续 handler chain fallthrough)
  - ${ACP}/src/agent-client-protocol/src/jsonrpc/incoming_actor.rs                (dispatch_dispatch 264-300：handler chain 顺序调用直到 Handled::Yes；空 chain / 全 No ⇒ 通过 report_handler_error 回 method_not_found 给对端)
  - ${LSP}/src/jsonrpc/router.rs                                                  (Router::method(name, callback, layer) 43-66 注册口；Service::call 87-95：methods.get_mut(method) → 命中 handler，**未命中自动 Response::from_error(Error::method_not_found())** —— 这是 zhive 默认行为的直接锚点)
references_internal:
  - plans/phase1-core-native-research/deliverables/C1-client-api.md §2.2  (ReverseHandler trait 草签 142-158；builder reverse_handler 224-228；ClientEvent::ServerRequest 旁路 172-173)
  - plans/phase1-core-native-research/deliverables/C1-client-api.md §4    (拓扑图 450-458：worker.classify() → method ∈ handler.methods() spawn handle；∉ ⇒ method_not_found + 仍 emit ServerRequest)
  - plans/phase1-core-native-research/deliverables/C1-client-api.md §6.3  (reverse 注册的 A3 method 表：permission/request / hook/run / session/request_user_input)
  - plans/phase1-core-native-research/deliverables/C3-client-cancel.md §3.5 (反向 RPC 在 cancel 期间命运：server 主动发 Cancelled outcome，client worker drop pending handle future)
  - plans/phase1-core-native-research/deliverables/B6-permission-reducer.md §4.3 (session/resume_permission 是 **client→server request**，不是 reverse；turn/suspended / turn/resumed 是 server→client **notification**)
non-goals:
  - 不写 zhive crate 源码（本 deliverable 所有 Rust 代码块为草图，`todo!()` 占位）
  - 不改 research/99-decisions/
  - 不暴露 zhive-core 类型（client 仅依赖 zhive-proto）
  - 不重定义 C1 已落定的 ReverseHandler trait 形态（C4 只深化 dispatch / 默认行为 / 多 method 表）
---

> 范围声明：C4 调研产出。在 C1 已锁的 `trait ReverseHandler { fn methods(&self) -> &[&'static str]; async fn handle(...) -> Result<Value, JsonRpcError> }` 上深化 dispatch 实现、默认行为、反向 method × 默认行为表。本文件不引入新 trait 形态。
> ${ACP} = `~/Desktop/code/github/acp-rust-sdk/`；${ACP_REG} = `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`；${LSP} = `~/Desktop/code/github/tower-lsp/`。

---

## 1. 参考点清单

| 论断 | 仓库 / 路径 | 行号 |
|---|---|---|
| ACP `permission/request` 是 **agent→client reverse RPC**（位于 `agent_to_client/requests.rs`） | `${ACP}/src/agent-client-protocol/src/schema/agent_to_client/requests.rs` | 9-13 |
| ACP `RequestPermissionRequest` 字段 `{ session_id, tool_call, options: Vec<PermissionOption>, _meta }` | `${ACP_REG}/agent-client-protocol-schema-0.12.0/src/client.rs` | 555-601 |
| ACP `RequestPermissionResponse { outcome: RequestPermissionOutcome, _meta }` | 同上 | 683-720 |
| ACP `RequestPermissionOutcome` 二态：`Cancelled` / `Selected(SelectedPermissionOutcome)` | 同上 | 722-739 |
| ACP **硬约束**：client MUST respond to all pending `session/request_permission` with `Cancelled` outcome 当收到 `session/cancel` | 同上 docstring | 727-734 |
| ACP method 名常量：`RequestPermissionRequest => "session/request_permission"` | `${ACP}/src/agent-client-protocol/src/schema/enum_impls.rs` | 75 |
| ACP `RequestHandler::handle_dispatch_from`：method 字符串匹配 `Req::matches_method`；不匹配 ⇒ `Handled::No retry=false` 让 chain 下一个 handler 接 | `${ACP}/src/agent-client-protocol/src/jsonrpc/handlers.rs` | 100-176（118 匹配 / 159-165 未匹配） |
| ACP `dispatch_dispatch`：handler chain 顺序调用直到 `Handled::Yes`；全 No 时 …… 经 `report_handler_error` 回 method_not_found | `${ACP}/src/agent-client-protocol/src/jsonrpc/incoming_actor.rs` | 264-300 |
| LSP `Router::method(name, callback, layer)` 注册口 + `Service::call` 未命中自动 `Error::method_not_found()` | `${LSP}/src/jsonrpc/router.rs` | 43-99（87-95 未命中分支） |
| LSP `MethodHandler<P, R, E>` boxed Fn 注册 + per-method layer 支持 | 同上 | 101-117 |
| C1 `ReverseHandler` trait 完整草签：`methods()` 返回 `&[&'static str]`；`handle(method, params)` 返回 `Result<Value, JsonRpcError>` | `deliverables/C1-client-api.md` | §2.2 142-158 |
| C1 worker 拓扑：method ∈ handler.methods() ⇒ spawn handle；∉ ⇒ MethodNotFound 应答（仍 emit `ClientEvent::ServerRequest` 让旁观者可看） | `deliverables/C1-client-api.md` | §4 450-458 |
| C1 §6.3 已枚举 3 个反向 method：`permission/request` / `hook/run` / `session/request_user_input` | `deliverables/C1-client-api.md` | §6.3 528-534 |
| C3 cancel 期间 server **主动**发 `Cancelled` outcome；client worker 收到 wire 应答后 drop pending handle future（不调 handle 后处理） | `deliverables/C3-client-cancel.md` | §3.5 230-234 |
| B6 `session/resume_permission` 是 **client→server request**（不是反向）；`turn/suspended` / `turn/resumed` 是 **server→client notification**（也不是反向 request） | `deliverables/B6-permission-reducer.md` | §4.2-4.3 254-353 |
| A4：14 hook event 由 **server 内部** dispatch（host 调本进程注册的 hook callback），不通过 client 反向 RPC | `deliverables/A4-hook-event-schema.md` | §1.1 + §0 摘要（"统一 RPC 反向请求 / hook host 单一调度路径，所有 hook callback 通过统一 register_hook 注册"） |

---

## 2. `ReverseHandler` trait 完整签名（在 C1 基础上明确）

> C1 §2.2 已落定 trait 形态。C4 把字面签名固化、补 invariant 与 docstring。**不变更 C1 的字段／返回类型**。

```rust
//! C4 草图：trait ReverseHandler 完整签名，附 method 列表存储语义、cancellation 接入、错误约定。
//! 落地点：crates/zhive-client-native/src/reverse.rs（Phase 1 引出 pub trait）

use std::sync::Arc;
use serde_json::Value;

use zhive_proto::JsonRpcError;  // = ErrorObject (zhive-proto/src/lib.rs:174-181)

/// 服务端 → 客户端反向 RPC 处理器。
///
/// **C1 已锁形态**（C4 仅补 docstring + invariant）。Worker 在每条入站
/// `ServerRequest` 上：
/// 1. 取 `req.method`；
/// 2. 调 `handler.methods()` 得到 `&[&'static str]`；
/// 3. 若 `req.method` ∈ 列表 ⇒ spawn `handler.handle(method, params)`；
/// 4. 否则 ⇒ 立即用 `JsonRpcError::method_not_found()` 应答（不调 `handle`）。
///
/// # 设计点
///
/// - **`methods()` 返回 `&[&'static str]`** 而不是 `Vec<String>` —— 注册期完全静态、零 alloc、且
///   命中检查可走 `slice.contains` linear scan（method 数 ≤ ~10，无 hash 收益）。
///   对齐 LSP `Router.methods: HashMap<&'static str, ...>` 的 key 类型选择（${LSP} router.rs:23）。
/// - **`handle` 接 `method: &str` + `params: Value`** —— caller 在 trait 实现里 `match method`
///   分发到具体 typed deserialize。原因：单 trait 实现承载多 method 时 typed signature 会 N 倍重复；
///   `Value` 一次 deserialize 成本可忽略（C1-Q3 已答辩，trait 优于 closure map）。
/// - **不接 `&CancellationToken` 参数**。理由：cancel 走 wire 协议（`session/cancel` → server 主动发
///   `Cancelled` outcome；C3 §3.5 已固化）。client worker 收到 server 发来的 Cancelled 应答后**直接 drop**
///   `handle` future，不再注入 cancellation token；caller 想 cooperative cancel ⇒ 自己在 `handle` body
///   用 `tokio::select!` 监听自己持有的 token（典型：UI 弹窗用户点 cancel）。
/// - **返回类型 `Result<Value, JsonRpcError>`**：`Ok(v)` ⇒ client 自动包 `{ result: v }` 应答；
///   `Err(e)` ⇒ 自动包 `{ error: e }`。**不暴露 wire envelope**（caller 不写 jsonrpc / id 字段）。
///
/// # 失败语义
///
/// | handle 返回 | wire 应答 |
/// |---|---|
/// | `Ok(value)` | `{ "jsonrpc": "2.0", "id": <id>, "result": <value> }` |
/// | `Err(JsonRpcError)` | `{ "jsonrpc": "2.0", "id": <id>, "error": <obj> }` |
/// | `handle` panic | worker `catch_unwind` ⇒ `Err(JsonRpcError::internal_error("handler panicked"))` |
/// | `handle` future 被 drop（cancel） | worker **不发应答** —— server 已主动发了 Cancelled outcome（C3 §3.5） |
///
/// # 示例（伪）
///
/// ```ignore
/// use std::sync::Arc;
/// use serde_json::Value;
/// use zhive_proto::JsonRpcError;
/// use zhive_client_native::ReverseHandler;
///
/// struct MyHandler;
///
/// #[async_trait::async_trait]
/// impl ReverseHandler for MyHandler {
///     fn methods(&self) -> &[&'static str] {
///         &["permission/request", "session/request_user_input"]
///     }
///
///     async fn handle(&self, method: &str, params: Value) -> Result<Value, JsonRpcError> {
///         match method {
///             "permission/request" => {
///                 // typed deserialize then UI prompt
///                 todo!()
///             }
///             "session/request_user_input" => {
///                 todo!()
///             }
///             // 不可达：worker 已用 methods() 过滤
///             other => Err(JsonRpcError::method_not_found_for(other)),
///         }
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait ReverseHandler: Send + Sync {
    /// 此 handler 声明能处理的 method 列表。
    ///
    /// **invariant**：返回值在 handler 生命周期内**保持不变**。worker 在注册期（builder
    /// `.reverse_handler(Arc<...>)` 时刻）拿一次拷贝缓存（见 §3 dispatch 伪码）。
    /// 若 caller 想运行时增删 method，必须 rebuild Client（**不支持热重注册**，C4-N3 未决）。
    fn methods(&self) -> &[&'static str];

    /// 处理一条反向请求。
    ///
    /// `method` 保证 ∈ `self.methods()` 返回值（worker 已 fast-path 过滤）；
    /// 实现里若 `match` 默认分支被命中，应返回 `JsonRpcError::internal_error` —— 这是 invariant 违反。
    ///
    /// `params` 是 wire JSON-RPC `params` 字段裸值（已剥 envelope）。
    async fn handle(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError>;
}
```

### 2.1 与 C1 草签的字面对照

| C1 §2.2 142-158 字面 | C4 草签 | 差异 |
|---|---|---|
| `fn methods(&self) -> &[&'static str];` | 同 | 0 |
| `async fn handle(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, JsonRpcError>;` | 同 | 0 |
| `#[async_trait::async_trait]` + `Send + Sync` | 同 | 0 |
| C1 docstring 提"在合理时间内返回（建议 ≤ 30s，与 A3 §10 TODO-A3-O3 timeout 对齐）" | C4 移除该建议（B6 §7 已落定默认 30s 在 **server 侧**，client 不强制） | C4 修订：timeout 由 server 主动发 Cancelled outcome 强制，client handler 无需自检 |
| C1 docstring 提"若 client 的 session/cancel 已触发，worker 会先用 Cancelled outcome 自动应答" | C4 修订：**实际**是 server 主动发 Cancelled 给 client（C3 §3.5）；client worker 收到该应答后 drop pending handle future，**不发**任何 wire 消息（避免双发） | C1 描述与 ACP 实际语义错位，C4 修正 |

> **C1 草签 vs C4 决策的唯一实质修订**：cancel 期间应答的发起方是 server（B7 §3.3）不是 client；client 端只 drop future。`handle` 实现内部如想 cooperative cancel，自带 token（C4-N1 未决：是否在 builder 上提供 `Arc<CancellationToken>` 注入接口）。

---

## 3. Dispatch 实现伪码

> Worker task 内部对每条入站 `ServerRequest` 的处理流程。锚点：C1 §4 拓扑图 450-458 + LSP `Router::call` 87-95 + ACP `dispatch_dispatch` 264-300。

### 3.1 注册期（Client builder → Client）

```rust
// builder.reverse_handler(Arc<dyn ReverseHandler>) 后 ClientBuilder::connect_* 时：

struct WorkerState {
    reverse_handler: Option<Arc<dyn ReverseHandler>>,
    // 注册期缓存的 method 列表（snapshot；invariant：handler 生命周期内不变）
    reverse_methods: Vec<&'static str>,
    // pending reverse RPC 应答（worker spawn handle 后把 join_handle 放这里；
    // server Cancelled 应答到达时 worker 用 req_id 找到 join_handle abort 之）
    pending_reverse: HashMap<RequestId, tokio::task::JoinHandle<()>>,
    // 其余 stdio/transport state
}

impl WorkerState {
    fn new(reverse_handler: Option<Arc<dyn ReverseHandler>>) -> Self {
        let reverse_methods = reverse_handler
            .as_ref()
            .map(|h| h.methods().to_vec())  // snapshot
            .unwrap_or_default();
        Self { reverse_handler, reverse_methods, pending_reverse: HashMap::new() }
    }
}
```

### 3.2 入站 ServerRequest 时的 classify + dispatch

```rust
// worker tokio::select! 收到 transport recv 一条 ServerRequest：
async fn dispatch_server_request(
    state: &mut WorkerState,
    transport_tx: &TransportTx,
    event_tx: &mpsc::Sender<ClientEvent>,
    req: ServerRequest,
) {
    let method = req.method.as_str();
    let req_id = req.id.clone();

    // 1) 旁路 emit（C1 §2.2 172-173 + C1-N5 未决：默认 false，未来开关）
    //    若 caller 开启 passthrough：先 emit 让上层观测；不影响应答路径
    if state.passthrough_events {
        let _ = event_tx.try_send(ClientEvent::ServerRequest(req.clone()));
    }

    // 2) classify
    let registered = state.reverse_methods.iter().any(|m| *m == method);

    if !registered {
        // 3) fast-path 拒绝：未声明 method ⇒ MethodNotFound + 不调 handle
        //    锚点 LSP router.rs:91-95（命中 None 分支 → Error::method_not_found）
        let err = JsonRpcError::method_not_found(method);
        transport_tx.send_error_response(req_id, err).await;
        return;
    }

    // 4) 命中：spawn handle 任务（cooperative，不持锁）
    let handler = state.reverse_handler.clone().expect("classified true implies handler set");
    let method_owned = method.to_string();
    let params = req.params;
    let tx = transport_tx.clone();
    let req_id_for_task = req_id.clone();

    let join = tokio::spawn(async move {
        // panic 边界：catch_unwind 防 handler panic 污染 worker
        let fut = std::panic::AssertUnwindSafe(handler.handle(&method_owned, params));
        let result = futures::FutureExt::catch_unwind(fut).await;
        match result {
            Ok(Ok(value)) => { tx.send_ok_response(req_id_for_task, value).await; }
            Ok(Err(jrpc_err)) => { tx.send_error_response(req_id_for_task, jrpc_err).await; }
            Err(_panic) => {
                let err = JsonRpcError::internal_error("reverse handler panicked");
                tx.send_error_response(req_id_for_task, err).await;
            }
        }
    });

    state.pending_reverse.insert(req_id, join);
}
```

### 3.3 入站 Cancelled 应答 / 断连 / drop 时的 cleanup

> **关键**：cancel 走 wire 时是 **server 主动发** `RequestPermissionResponse { outcome: Cancelled }` 给 client，**而不是** client 自己应答 Cancelled（C3 §3.5；ACP schema 727-734 verbatim "client MUST respond ... with Cancelled" 是描述 zhive-server 侧职责）。

| worker 状态 | 触发事件 | 操作 |
|---|---|---|
| `pending_reverse[req_id]` 存在；server 发回 Cancelled outcome | transport recv 到 `Response { id, result: { outcome: "cancelled" } }` 给某条 reverse req | 这条 wire 应答**不是** client 给 server 的，是 server 主动给 client 的。worker 应**忽略**它作为响应方向的语义（client 不在等响应）；同时 `pending_reverse.remove(req_id).abort()` 让 handle future 立即 drop。caller 注册的 cooperative cancel 立即生效；否则跑完丢弃返回值（不发任何应答 —— wire 上 server 自己已发过应答）。 |
| `Client::shutdown()` / drop | transport close | 遍历 `pending_reverse.drain() → join.abort()`；不发任何 wire 应答（server 通过 transport EOF 推断 client 走人） |
| `handle` 自然返回 `Ok(_)` 或 `Err(_)` | tokio task 结束 | spawn 内已 `tx.send_*_response`；worker 这里通过 `JoinHandle::poll` 完成 + `pending_reverse.remove(req_id)` |
| handler panic | `catch_unwind` 捕获 | spawn 内 `tx.send_error_response(.., internal_error)`；同上 remove |

### 3.4 method 不匹配的 fallthrough 行为汇总

| caller 场景 | C4 实际行为 |
|---|---|
| caller **未** 在 builder 上 `.reverse_handler(...)` | `state.reverse_handler = None`，`state.reverse_methods = []`；任何 ServerRequest ⇒ `MethodNotFound` 应答 + (可选)emit `ClientEvent::ServerRequest` |
| caller 注册了 handler，但 `methods()` 返回的 slice 不含此 method | 同上：fast-path `method_not_found`，**不调** `handle`（避免 caller 实现里写 panic 兜底） |
| caller 注册了 handler，`methods()` 含此 method，`handle` 自己返回 `Err(JsonRpcError::method_not_found_for(...))` | 走 caller 显式 error 路径 —— wire 上确实回 method_not_found，但语义是"我能处理这族 method，但这条具体不行"。caller 自负其责，C4 不区分这两条路径 |
| caller 实现 `handle` 时 `match method { ... _ => panic!("invariant: worker 已过滤") }` | `catch_unwind` 转 `internal_error` 应答 —— 是 invariant 违反，应在测试中被发现 |

> 锚点对比：LSP `Router::call`（router.rs:87-95）"if let Some(handler) = methods.get_mut(method) { handler.call() } else { method_not_found }"是**单 router** 模型，zhive `ReverseHandler` 是**单 handler 多 method**（trait 内部 match），dispatch 拓扑等价（worker 持 `methods` slice 作为 router；trait 实现自带匹配）。

---

## 4. 反向 method × 默认行为 表

> Phase 1 zhive client 已知反向 method 的全集 + 各自默认行为。锚点：C1 §6.3、A3 §6.3、B6 §4.3、A4 §1（hook 不走反向）。

| Method | 方向 | 类型 | C1 是否枚举 | C4 默认行为（未注册时） | 备注 |
|---|---|---|---|---|---|
| `session/request_permission` | server → client | request | 是（C1 §6.3 `permission/request` —— **method 名字面有歧义**，见下表底部修订） | `JsonRpcError::method_not_found` | **ACP 0.12 字面**；C1 §6.3 写的 `permission/request` 与 ACP `session/request_permission`（enum_impls.rs:75）字面**不一致**。C4 决策：**采纳 ACP 字面 `session/request_permission`**，C1 §6.3 第一行需在 decision-diffs.md 修订。详见 §4.3 |
| `session/request_user_input` | server → client | request | 是（C1 §6.3） | `method_not_found` | codex `ToolRequestUserInputParams` 移植（C1 引 lib.rs:957-958）；ACP 0.12 schema 未提供 —— zhive 私有扩展。未注册时 client 拒绝 ⇒ server 应在 capabilities 协商时探测此能力（A2 capabilities 已含 `permission` flag，未来加 `userInput` flag） |
| `hook/run` (C1) / `hook/invoke` (本任务 brief) | **不存在反向 method** | — | C1 §6.3 写"hook/run"是误录 | **N/A —— client 永远不会收到此 method** | **关键发现**：A4 §1 + B5/B6 均把 hook dispatch 定为 **server 内部** —— hook host 跑在 server 进程内（C1 引 A4 概述 §0）。client 不参与 hook callback dispatch。C4 决策：**反向 method 表中删除 `hook/run`**（C1 §6.3 第二行需在 decision-diffs.md 修订） |
| `fs/read_text_file` / `fs/write_text_file` | server → client | request | 否 | `method_not_found` | ACP 0.12 reverse RPC（${ACP}/...agent_to_client/requests.rs:14-23）；Phase 1 zhive 不实装（D-002 锁定 stdio + uds 为 Phase 1 transport，文件读写不通过反向 RPC 走，zhive engine 直接在 server 进程内 IO）。未来若做 sandboxed agent，可由 caller 注册 handler 实装 |
| `terminal/create` / `terminal/output` / `terminal/release` / `terminal/wait_for_exit` / `terminal/kill` | server → client | request | 否 | `method_not_found` | ACP 0.12 reverse RPC 5 个（${ACP}/...agent_to_client/requests.rs:24-44）；Phase 1 zhive 不实装。同上 |
| 任何未列入 method | server → client | request | 否 | `method_not_found` | **总兜底**：worker 见 `state.reverse_methods.contains(method) == false` ⇒ 一律 method_not_found，**不 panic / 不 deny / 不静默丢弃** |

### 4.1 默认行为为何选 `method_not_found` 而不是 `Deny`

| 备选默认 | 选 / 不选 | 理由 |
|---|---|---|
| **A. `method_not_found` JSON-RPC error code -32601**（C4 选） | ✅ | (a) 标准 JSON-RPC 错误码语义最准；(b) server 收到此 error 可决定降级（如：未注册 `session/request_permission` ⇒ server 走默认 `Deny` reducer 而不阻塞 turn）；(c) 对齐 LSP router.rs:91-95 unmapped method 默认行为 + ACP dispatch_dispatch 全 handler 拒后的 report_handler_error 路径 |
| B. 自动构造 `PermissionDecision::Deny` JSON 应答（按 method 类型走 typed 兜底） | ❌ | (a) client 不应擅自做 server 的语义决策（client 只知道协议层，不知道 permission scope）；(b) `session/request_user_input` 没有 `Deny` 概念；(c) "deny 兜底"会让 server 误以为 client 在线参与决策，掩盖配置错误 |
| C. panic / abort worker | ❌ | client 是库 crate，panic 会污染 caller runtime；C1-Q4 已决"永不 panic" |
| D. 静默丢弃（不发应答） | ❌ | server 端 pending Map 永远不清，turn 挂死；违反 ACP "请求必应答"语义 |
| E. 分 method 给不同 default（如 permission/request ⇒ Deny；user_input ⇒ method_not_found） | ❌ | client 不持有 method × 默认决策表（语义在 server 侧 + caller policy）；client 唯一可靠 default 是协议层错误码 |

### 4.2 server 侧拿到 `method_not_found` 后的语义降级路径（非 C4 范围，供对照）

> 这部分**由 B6 / B5 决定**，C4 仅说明 client 行为产物如何被 server 消费：

- server 发出 `session/request_permission` reverse-RPC 期望 client 帮忙弹 UI；
- 若 client 应答 `JsonRpcError { code: -32601, message: "method not found", data: "session/request_permission" }`；
- server 应降级为 `PermissionDecision::Deny`（安全默认）+ 不再对此 client 发同 method（per-connection cache 一次性失败）；
- 同步 turn 通过 `session/aborted` 或 `turn/completed { failed }` 走完。

### 4.3 C1 §6.3 反向 method 列表修订（送 decision-diffs.md）

| C1 §6.3 写的字面 | C4 实际锚定 | 修订建议 |
|---|---|---|
| `"permission/request"` | ACP `session/request_permission`（enum_impls.rs:75；client.rs:557 `SESSION_REQUEST_PERMISSION_METHOD_NAME`） | **改为 `"session/request_permission"`**。理由：与 ACP 0.12 wire 字面对齐，避免 zhive 私有命名与 ACP bridge 翻译层 |
| `"hook/run"` | **不存在**：A4 + B5 + B6 都将 hook dispatch 定为 server 内部 | **删除此行**。hook 由 server `HookHost` 调本进程注册的 callback，不走反向 RPC |
| `"session/request_user_input"` | codex 移植，无 ACP 锚点 | 保留为 zhive 私有反向 method。未来 Phase 2 评估是否提交 ACP 标准 |

> TODO(开放项 C4-N2)：上述 3 条 C1 §6.3 修订在 `decision-diffs.md` 集中提交。本 deliverable **不**直接改 C1。

---

## 5. 与 ACP `permission/request` 形状的对齐验证

> 验证 zhive `ReverseHandler::handle("session/request_permission", params)` 的 params/return 形状是否与 ACP 0.12 `RequestPermissionRequest`/`RequestPermissionResponse` 1:1 对齐。

### 5.1 入参 `params` ⇄ `RequestPermissionRequest`

| ACP 字段 | 类型 | zhive `handle` 内反序列化路径 |
|---|---|---|
| `session_id: SessionId` | `Arc<str>` newtype | `params.get("sessionId")` ⇒ `SessionId`（zhive-proto 同名 type，A1 决） |
| `tool_call: ToolCallUpdate` | struct（包含 tool name / input / kind / status） | `params.get("toolCall")` ⇒ `ToolCallUpdate`（zhive-proto 镜像 ACP schema） |
| `options: Vec<PermissionOption>` | 含 `{ option_id, name, kind: AllowOnce/AllowAlways/RejectOnce/RejectAlways }` | `params.get("options")` ⇒ `Vec<PermissionOption>` |
| `_meta: Option<Meta>` | extensibility bag | optional `.get("_meta")` |

**zhive 选型**：handler 实现里通过 `serde_json::from_value::<RequestPermissionRequest>(params)?` 一次 typed deserialize。**zhive-proto 暴露的是 ACP 0.12 schema 的镜像 type**（A2 + B4 落地），不重命名字段。

### 5.2 返回 `Result<Value, JsonRpcError>` ⇄ `RequestPermissionResponse`

| ACP 字段 | zhive `handle` Ok 路径返回 |
|---|---|
| `outcome: RequestPermissionOutcome::Cancelled` | **不应由 client handle 主动返回** —— Cancelled 是 server 主动发的 wire 应答语义（ACP 727-734 + C3 §3.5）。client handler 若 caller 想表达"user 撤销"，应通过 `JsonRpcError::request_cancelled (-32800)` 而不是 Selected。**B6 落定**：把 `Cancelled` outcome 作为 wire 上**只能从 server 出**的 variant；client handler 永远走 `Selected` |
| `outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, _meta })` | `serde_json::to_value(&RequestPermissionResponse { outcome: Selected(SelectedPermissionOutcome::new(option_id)), meta: None })` |
| `_meta: Option<Meta>` | caller 可填 |

### 5.3 Cancelled outcome 路径（C3 + B6 + ACP 三方核对）

```
caller (zhive client embedder)        zhive-client-native worker         zhive-server
       │ register ReverseHandler          │                                  │
       │ ──────────────────────────────►  │                                  │
       │                                  │                                  │
       │ user 调 client.cancel_session()  │                                  │
       │ ────────────────────────────────►│ notify("session/cancel", {sid})  │
       │                                  │ ────────────────────────────────►│ B7 路径：
       │                                  │                                  │   - abort_token.cancel()
       │                                  │                                  │   - drain pending_approvals
       │                                  │                                  │     foreach (req_id, _):
       │                                  │  ◄────────────────────────────── │       send Response{
       │                                  │                                  │         id: req_id,
       │                                  │                                  │         result: {outcome:"cancelled"}
       │                                  │                                  │       }
       │                                  │                                  │
       │                                  │ worker.classify_response():       │
       │                                  │   - 是 server 主动发给 reverse   │
       │                                  │     req 的应答（client 不在等）   │
       │                                  │   - pending_reverse.remove(req_id)│
       │                                  │   - join_handle.abort()           │
       │                                  │   - 不发任何 wire 消息            │
       │                                  │                                  │
       │ handle future drop                │                                  │
       │ (caller cooperative cancel?       │                                  │
       │  自行通过 tokio::select! 监听     │                                  │
       │  自带 token)                      │                                  │
       │                                  │ recv server/aborted notification │
       │                                  │ ◄────────────────────────────────│
       │ ClientEvent::Notification(...)   │                                  │
       │ ◄────────────────────────────────│                                  │
```

**核对结论**：ACP 0.12 schema 行 727-734 "client MUST respond ... with Cancelled" 在 zhive 拓扑里由 **server 代笔**：server 在 cancel 处理路径上**主动**给 client 发 Cancelled wire 应答，client worker 收到后 drop handle future。形态上仍满足 ACP wire 要求（client 端口确实回了 Cancelled 字面 wire），只是发起方在 zhive 拓扑里换成了 server —— 因为 zhive 模型下 server 持有 pending_approvals + `oneshot::Sender<PermissionDecision>`，client 没有独立的 "in-flight reverse list to drain"（C1-N5 旁路 event 不算）。

---

## 6. 关键问题逐条作答

### Q1：handler 注册接口（trait / `register_handler(method, fn)` / typed）

**trait**（C1 已选）。C4 给出 trait method 签名 + 多 method 时的 dispatch 实现伪码：

- 签名（§2 草签）：`fn methods(&self) -> &[&'static str]` + `async fn handle(&self, method: &str, params: Value) -> Result<Value, JsonRpcError>`。
- 多 method dispatch：单 handler trait 内部 `match method { ... }`（caller 自实现）；worker 用 `state.reverse_methods: Vec<&'static str>` 注册期 snapshot 做 O(N) 命中检查（N ≤ 10，linear 优于 hash）。锚点 LSP router.rs:23 / 87-95。
- 不选 typed-per-method（如 `Builder::on_permission_request(F)` + `on_user_input(F)`）：会随 method 表线性扩展 API surface，违反"协议中性"原则（C1 §3.2 选 `request_typed::<P,T>(&str, &P)` 的同源理由）。

### Q2：handler 执行环境（同步 / async / spawn_blocking）

**async + 独立 `tokio::spawn`**。理由：

- `handle` 已 `async`（trait 签名带 `async_trait::async_trait`），与 caller UI 弹窗 / 跨 task 通信对齐。
- 每条入站 `ServerRequest` 走 `tokio::spawn(handler.handle(...))`（§3.2 伪码），避免阻塞 worker 主 select_loop。多条 reverse req 可并行。
- **不**用 `spawn_blocking`：handler 内部 IO 是异步（UI event channel / typed deserialize），非 CPU-bound；若 caller 自己有阻塞操作（如阻塞 stdin 读 user 输入），应在 handler 实现里自己 `spawn_blocking` —— C4 不替 caller 做选择。
- 阻塞 handler 的副作用：worker 主循环不受影响；同一 caller handler 内部对**同 method**的并发由 caller 自己控制（如 UI 一次只能弹一个 permission 窗口 ⇒ caller 在 handler 内放 `Mutex`）。

### Q3：未注册的 reverse method 默认行为

**`JsonRpcError::method_not_found` (-32601)**，对所有未注册 method 一律此 default（不分 method 给不同 default）。锚点：LSP `Router::call` router.rs:91-95 的 unmapped method 默认 + ACP `dispatch_dispatch` incoming_actor.rs:264-300 chain 全 No 后的 report_handler_error。理由完整论证见 §4.1 表格。

- 不选 `Deny`：client 不持决策语义（§4.1 行 B）
- 不选 `panic`：库 crate 不污染 caller runtime（§4.1 行 C）
- 不选 静默丢弃：违反"请求必应答"语义，server pending Map 挂死（§4.1 行 D）
- 不选 分 method 不同 default：决策语义在 server / caller 侧，client 仅承担协议层兜底（§4.1 行 E）

---

## 7. 未决项

> TODO(开放项 C4-N1)：handler `handle` 是否应接 `&CancellationToken` 形参？C4 现选**不接**（caller 自己在 builder 上注入 `Arc<CancellationToken>` 或在 handler struct 字段持有）。但 UX 上 caller 可能希望"零仪式 cooperative cancel"。备选：`fn methods()` 返回 `&[&'static str]` 不变；`handle` 改为 `async fn handle(&self, method: &str, params: Value, cancel: &CancellationToken) -> Result<Value, JsonRpcError>`。worker 在收到 server-side Cancelled 应答时 `cancel.cancel()` 让 handler 优雅退出。建议 Phase 2 实测 caller 痛点后再回归此决策（不破坏现有 trait 形态 —— 只需多一个 default-impl 适配层）。

> TODO(开放项 C4-N2)：C1 §6.3 反向 method 列表的 3 条修订（`permission/request` → `session/request_permission`；删除 `hook/run`；保留 `session/request_user_input`）送 decision-diffs.md，集中回流。

> TODO(开放项 C4-N3)：handler 热重注册（caller 想运行时换 `Arc<dyn ReverseHandler>`）。当前 C4 选**不支持**（builder 一次性注册；rebuild Client 才能换）。理由：(a) `state.reverse_methods` 是注册期 snapshot；(b) 热替会引入 race（in-flight handle future 持旧 handler refcount）。Phase 2 若 caller 强需求（动态 plugin 加载），考虑 `RwLock<Arc<dyn ReverseHandler>>` + 注册期 cache invalidation。

> TODO(开放项 C4-N4)：多 handler 串联（caller 想为 `permission/*` 和 `session/request_user_input` 注册不同 struct）。C1-N2 已记。C4 维持单 handler：caller 自己写"合并 handler"（`struct CompositeHandler { permission: Arc<dyn ReverseHandler>, input: Arc<dyn ReverseHandler> }` + `match method` 路由）。若 caller 强需求，Phase 2 可加 `builder.reverse_handlers(Vec<Arc<dyn ReverseHandler>>)` —— invariant：method 集合互斥（重叠时 builder 阶段 error）。

> TODO(开放项 C4-N5)：`session/request_user_input` 不在 ACP 0.12 schema。是否补 ACP-style schema 提交上游？C4 倾向 Phase 2 评估（先在 zhive 私有用，沉淀使用反馈后再上）。

> TODO(开放项 C4-N6)：handler `handle` panic 时 worker `catch_unwind` 返回 `internal_error` 是否暴露 panic message？倾向**不暴露**（PII / 堆栈泄漏风险），但 `tracing::error!` 记录完整 panic info 到 server 端日志。具体 wire `JsonRpcError.data` 字段填 `{ "kind": "handler_panic" }` 即可，不带 message。

> TODO(开放项 C4-N7)：worker 旁路 emit `ClientEvent::ServerRequest`（C1-N5）的默认开关。C4 暂定**默认关**（避免双语义），caller 走 `.reverse_handler_passthrough_events(true)` 显式开启。文档需说明：开启后 caller 同时通过 `next_event` 与 `ReverseHandler::handle` 看到同一条请求，**只有 handle 的返回值用于 wire 应答**（next_event 仅观测用，caller 不应自己 resolve）。

---

## 8. 验收对照

- [x] 论断带锚点（§1 参考点 + 文中行号 verbatim 引用 ACP/LSP/C1/C3/B6/A4）
- [x] 不动 `crates/` 源码（本 deliverable 所有 Rust 代码块为草图，`todo!()` 占位 + spawn/await 伪码）
- [x] 不改 `research/99-decisions/`（C1 §6.3 修订列入 §7 C4-N2 决策回流）
- [x] 不 `git pull`（ACP 读 `agent_to_client/requests.rs` + `enum_impls.rs` + `role/acp.rs` + `jsonrpc/handlers.rs` + `jsonrpc/incoming_actor.rs` + cargo registry schema 0.12.0/client.rs；LSP 读 `jsonrpc/router.rs`）
- [x] 不在 C1 trait 形态上做实质变更（§2.1 字面对照零差异，仅修订两条 docstring）
- [x] handler 执行环境 = **async + per-request `tokio::spawn`**（§3.2 + §6.Q2）
- [x] 未注册 method 默认 = **`JsonRpcError::method_not_found`**（§4 + §6.Q3）
- [x] 反向 method 表覆盖 `session/request_permission` / `session/request_user_input` / `fs/*` / `terminal/*` / 未知（§4）
- [x] 与 ACP `permission/request` 形状对齐 + Cancelled outcome 路径（§5）
- [x] 25-40 分钟内落盘

— C4 deliverable end —
