---
task: B7
title: 取消传播树 + StreamingBehavior 三队列状态机 + pendingSessionWrites 智能刷新
plan: phase1-core-native-research
date: 2026-05-28
status: implemented
depends_on:
  - A3 deliverable (三队列 + QueueMode + Steer 不撤当前 tool_call + ACP Cancelled outcome)
  - B1 deliverable (Engine actor + EnginePhase 5 态 + watch::Sender<EnginePhase> + CancellationToken 选型 + broadcast(1024) / mpsc / oneshot 拓扑)
  - research/99-decisions/README.md#d-008 (StreamingBehavior 二元 mode —— 本任务沿 A3 推进三队列修订)
references_external:
  - ${ACP}/src/agent-client-protocol/src/schema/client_to_agent/notifications.rs:1-3       (`CancelNotification` impl with method `"session/cancel"`)
  - ${ACP}/src/agent-client-protocol/src/mcp_server/builder.rs:452-473                     (`pin!(context.ct.cancelled())` + 返回 `Err(rmcp::ErrorData::internal_error("operation cancelled", None))`)
  - ${ACP}/src/agent-client-protocol-cookbook/src/lib.rs:170-188                           (`RequestPermissionOutcome::Cancelled` 在 None 时返回；以及 `RequestPermissionOutcome::Selected { option_id }`)
  - ${LSP}/src/service/pending.rs:14-78                                                    (`Pending(Arc<DashMap<Id, AbortHandle>>)` + `execute(id, fut)` 在 cancel 后返回 `Response::from_error(id, Error::request_cancelled())` + `cancel_all`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:174                                  (`private pendingSessionWrites: PendingSessionWrite[] = []` 字段持有)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:439-481                              (`prepareNextTurn` 起 `flushPendingSessionWrites()`，实现按 `write.type` 分发到 `appendMessage / appendModelChange / appendThinkingLevelChange / appendCustomEntry / appendCustomMessageEntry / appendLabel / appendSessionName / setLeafId`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:483-510                              (`handleAgentEvent` 在 `turn_end` 起 `flushPendingSessionWrites()` 并 emit `save_point { hadPendingMutations }`；`agent_end` 也起 flush 并切 idle)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:552-600                              (`runAbortController = new AbortController()`；`runAgentLoop(..., abortController.signal, ...)` 主循环显式接 signal；`finally` 块 `flushPendingSessionWrites()`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:669-679                              (`appendMessage(message)` 在 `phase==="idle"` 直写，否则 push 到 buffer)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:681-735                              (`compact()`：phase=compaction；hook payload `signal: new AbortController().signal` —— 一次性 signal，不挂主 abort 树)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:737-833                              (`navigateTree()`：phase=branch_summary；`const signal = new AbortController().signal` 同样一次性)
  - ${PI}/packages/agent/docs/hooks.md:21-32                                               (hook handler signature `(event, ctx, signal?: AbortSignal) => ResultOf<E> | Promise<...>` —— signal **可选**)
references_internal:
  - plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md   §4-§8 (三队列 + abort + nextTurn 保留 + reverse-RPC Cancelled outcome)
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md                    §6.1-§6.4 (Engine actor + channel 拓扑 + ThreadHandle/ActiveTurn.cancel: CancellationToken + broadcast(1024))
non-goals:
  - 写 zhive crate 源码（本任务仅 markdown deliverable）
  - 修订 99-decisions/（如需修订走 plan §10 `decision-diffs.md` 集中回流）
  - 重新设计 Pi `session.appendXxx` API；zhive 复刻其语义即可
---

> **决策衔接警告（D-008 / A3）**：A3 已把 `StreamingBehavior` 从 D-008 的"二元 mode"修订为"三队列模型"。本 deliverable 直接继承 A3 三队列假设。**与 R5 finding #1 假设不同**的最关键一点：A3 + B7 共同确认 **Steer 不撤销 in-flight tool_call**，详见 §3 时序图与 §7 对外文档措辞。R5 finding #1 若隐含"Steer 等价于 abort 当前 turn"应回头修订。

---

## 1. 参考点清单（含已修锚点）

| 主题 | 路径 | 行号 |
|---|---|---|
| ACP `session/cancel` notification 形状 | `${ACP}/src/agent-client-protocol/src/schema/client_to_agent/notifications.rs` | 1-3 |
| ACP MCP server 内部 cancel：`pin!(context.ct.cancelled())` + 返回 `operation cancelled` 内部错误 | `${ACP}/src/agent-client-protocol/src/mcp_server/builder.rs` | 452-473 |
| ACP `RequestPermissionOutcome::Cancelled` / `Selected { option_id }`（cookbook 用法 verbatim） | `${ACP}/src/agent-client-protocol-cookbook/src/lib.rs` | 170-188 |
| tower-lsp 反向请求 pending Map：`Pending(Arc<DashMap<Id, AbortHandle>>)` + `execute` 在 cancel 时返回 `Error::request_cancelled()` + `cancel_all` 用 `retain` 一次清空 | `${LSP}/src/service/pending.rs` | 14-78 |
| Pi `pendingSessionWrites: PendingSessionWrite[]` 字段持有 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 174 |
| Pi `flushPendingSessionWrites()` 按 `write.type` 分发到 8 个 `session.appendXxx` 方法 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 459-481 |
| Pi `prepareNextTurn` 起 flush（save point #1：下一轮 LLM 请求前） | `${PI}/packages/agent/src/harness/agent-harness.ts` | 439-441 |
| Pi `handleAgentEvent` 在 `turn_end` 起 flush + emit `save_point { hadPendingMutations }`（save point #2） | `${PI}/packages/agent/src/harness/agent-harness.ts` | 489-500 |
| Pi `handleAgentEvent` 在 `agent_end` 起 flush + 切 `phase = "idle"`（save point #3） | `${PI}/packages/agent/src/harness/agent-harness.ts` | 502-508 |
| Pi `executeTurn` `finally` 块 flush（save point #4：返回前最后保险） | `${PI}/packages/agent/src/harness/agent-harness.ts` | 594-600 |
| Pi `appendMessage(message)` 智能分发：`phase==="idle"` 直写，否则 push buffer | `${PI}/packages/agent/src/harness/agent-harness.ts` | 669-679 |
| Pi `compact()` hook payload `signal: new AbortController().signal` —— 一次性 signal **不挂主 abort 树** | `${PI}/packages/agent/src/harness/agent-harness.ts` | 701 |
| Pi `navigateTree()` `const signal = new AbortController().signal` —— 同样一次性 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 759, 774 |
| Pi hook handler signature `(event, ctx, signal?: AbortSignal)` —— signal **optional** | `${PI}/packages/agent/docs/hooks.md` | 21-32 |
| Pi `emitHook` / `emit` 内部把 signal 透传 observers / handlers | `${PI}/packages/agent/docs/hooks.md` | 124-149 |
| B1 决定：`ActiveTurn.cancel: CancellationToken` + `ThreadHandle.cancel: CancellationToken` + `shutdown: CancellationToken`（tokio_util） | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 695-705, 725 |
| B1 决定：event broadcast 容量 1024 / submission mpsc(512) / per-thread mpsc(64) / item mpsc(256) | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 716-723 |
| A3 决定：abort 清 steer/followUp、保留 nextTurn；pending session/request_permission 用 ACP `Cancelled` outcome 显式回 | `plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md` | §6.2-§6.3, §8 |
| A3 修订锚点（plan §4 修复）：`packages/agent/src/types.ts:44`（不是 `harness/types.ts:44`） | `${PI}/packages/agent/src/types.ts` | 44 |

---

## 2. 取消传播树（含 compaction / branch_summary 长操作分支）

### 2.1 总图（ASCII）

```
                          ┌────────────────────────────────┐
                          │     Engine.shutdown:           │
                          │     CancellationToken (root)   │
                          └────────────────────────────────┘
                                       │
            ┌──────────────────────────┴───────────────────────────┐
            │ child_token() per thread                              │
            ▼                                                       ▼
   ┌─────────────────────┐                                   ┌─────────────────────┐
   │ ThreadHandle.cancel │  …  thread N                       │ ThreadHandle.cancel │
   │ CancellationToken   │  (independent agent loop tasks)    │ CancellationToken   │
   └─────────────────────┘                                   └─────────────────────┘
            │ child_token() per turn
            ▼
   ┌─────────────────────┐
   │ ActiveTurn.cancel   │  (per-turn；steer 不 cancel 它，abort 才 cancel)
   │ CancellationToken   │
   └─────────────────────┘
            │ .clone() 显式克隆下传（cheap，Arc-backed）
            ├────────────────────────────────────────────────┐
            ▼                                                ▼
   ┌─────────────────────┐                          ┌─────────────────────┐
   │ Provider stream req │                          │ ToolCall execution  │
   │ (cancel = drop fut) │                          │ (cancel = abort     │
   └─────────────────────┘                          │  child process /    │
                                                    │  drop request fut)  │
                                                    └─────────────────────┘
                                                              │
                                                              ▼
                                                    ┌─────────────────────┐
                                                    │ Hook host emit()    │
                                                    │ 把 token 透传给     │
                                                    │ hook handler 第 3 参│
                                                    └─────────────────────┘
                                                              │
                                                              ▼
                                                    ┌─────────────────────┐
                                                    │ Subagent spawn      │
                                                    │ child engine = new  │
                                                    │ ActiveTurn 子树     │
                                                    │ (root = parent      │
                                                    │  ActiveTurn.cancel) │
                                                    └─────────────────────┘

   ┌────────────────────────────────────────────────────────────────────┐
   │  长操作分支（Pi 模式：不挂主 abort 树，自己用临时 token）           │
   │                                                                    │
   │  ┌──────────────────────┐         ┌──────────────────────┐         │
   │  │ Engine.compaction    │         │ Engine.branch_summary│         │
   │  │ CancellationToken    │         │ CancellationToken    │         │
   │  │ (NEW, 独立)          │         │ (NEW, 独立)          │         │
   │  └──────────────────────┘         └──────────────────────┘         │
   │  ── 仅由 compaction-cancel /      ── 仅由 navigate-cancel /         │
   │     shutdown 触发                    shutdown 触发                  │
   │                                                                    │
   │  设计偏离 Pi：Pi 直接 `new AbortController().signal` 永远 not       │
   │  aborted（一次性，无法外部触发）。zhive 改为有名字段 + 暴露         │
   │  `cancel_compaction()` 入口 + 父链 `shutdown` 触发取消。            │
   │  Pi 锚点：agent-harness.ts:701, 759, 774                            │
   └────────────────────────────────────────────────────────────────────┘
```

### 2.2 token 选型与传播规则

| 节点 | 类型 | clone or channel | 来源 / 父节点 |
|---|---|---|---|
| `Engine.shutdown` | `CancellationToken` (root) | — | `CancellationToken::new()` |
| `ThreadHandle.cancel` | `CancellationToken` | `.child_token()` of `shutdown` | 父：`Engine.shutdown` |
| `ActiveTurn.cancel` | `CancellationToken` | `.child_token()` of `ThreadHandle.cancel` | 父：thread |
| Provider stream / tool call | 直接 `.clone()` of `ActiveTurn.cancel` | clone | cheap，内部 `Arc` |
| Hook handler 第 3 参 | 同上 clone | clone | 显式作为 `Option<&CancellationToken>` 传 §6 |
| Subagent child engine | child engine 自己的 `ActiveTurn.cancel = parent_turn.cancel.child_token()` | child_token | 父：parent turn |
| **Compaction 长操作** | `Engine.compaction_cancel: CancellationToken`（独立有名字段） | `.child_token()` of `shutdown` | 父：`shutdown` **不挂当前 turn**（compact() 要求 phase==idle，无 active turn） |
| **BranchSummary 长操作** | `Engine.branch_summary_cancel: CancellationToken`（独立有名字段） | `.child_token()` of `shutdown` | 父：`shutdown`（同上） |

**channel 而非 token 的位点**：
- `Submission` mpsc(512)：input flow，**不参与 cancel 传播**（cancel 后 submission 仍可入队，由 dispatcher 决定如何拒绝）
- `EngineEvent` broadcast(1024)：output flow，**不参与 cancel 传播**（fan-out 永远活着，订阅方自己决定何时 drop receiver）
- `pending_approvals: HashMap<RequestId, oneshot::Sender<...>>`：cancel 时由 host 显式遍历 `take` 并 `send(Cancelled outcome)`（§4），**不**靠 token select；oneshot 本身 cancel-aware 但要先 take 才能 send

**关键点**：cancel 传播 = `CancellationToken` 父子关系树；input/output stream = channel。两者 orthogonal，B1 已奠基。

---

## 3. Steer / FollowUp / NextTurn 三队列时序图（含 Cancelled outcome 回收点）

### 3.1 Steer 时序（Pi 模式：**不撤销 in-flight tool_call**）

```
phase = Turn                                       steer_queue   in-flight
  │                                                              tool_call
  │  drain steer (空) → spawn LLM req
  │  stream chunks ...                                  []           -
  │  tool_call("run_tests")                             []           -
  │  ── reverse-RPC session/request_permission id=A ──► []           -
  │     pending_approvals[A] = oneshot::Sender
  │
  │  ◄── client: enqueue_steer("also run lint")
  │     phase==Turn ⇒ steer.push                       [lint]        -
  │     emit queue_update broadcast                     │            │
  │                                                     │            │
  │  ◄── client: session/request_permission response Allow ─        │
  │     pending_approvals.remove(A).send(Selected)      │            │
  │  ── tool exec begins (real syscall fired) ────►    [lint]      ⚙ run_tests
  │                                                     │            │
  │  ── tool result arrives ◄──────────────────────    [lint]        ✓
  │                                                     │
  │  inner_loop tick：drain steer = [lint]              []        (now)
  │     splice 到 messages 前 → 下一轮 LLM 请求 input    │
  │  ── LLM stream req with [lint, run_tests, ...] ──►  │
  │     LLM 此时才"看到" lint 指令
  │
turn_end → flush pending_session_writes →
emit save_point{ hadPendingMutations: bool }
```

**核心断言**：steer 在 `t = tool_call("run_tests")` 之后入队，但 `run_tests` **继续执行直到完成**；steer 内容仅影响**下一轮**（同一 turn 的 inner-loop）LLM 请求的输入。**没有任何 Cancelled outcome 在 steer 路径上发生**。

### 3.2 FollowUp 时序（turn 即将结束时续命）

```
phase = Turn                                  follow_up_queue
  │  ... agent stream 完成，无 more tool_call
  │  agent loop 即将退出 inner-loop
  │
  │  ◄── client: enqueue_follow_up("also check perf")
  │     phase==Turn ⇒ follow_up.push                [perf]
  │     emit queue_update
  │
  │  drain follow_up = [perf]                       []
  │  if drained.non_empty():
  │     pendingMessages = drained → 续 inner-loop
  │     ── new LLM stream req with [perf] ──►
  │
  │  ... 继续到自然 stop
turn_end → flush pending_session_writes
```

**核心断言**：FollowUp 在 turn 内 inner-loop 出口续命，**不退出 turn**；不触发任何 cancel。

### 3.3 NextTurn 时序（含 abort 保留 + Cancelled outcome 回收）

```
phase = Turn                  next_turn_queue   pending_approvals
  │  ... 工具调用进行中 ...                            { A → oneshot }
  │
  │  ◄── client: enqueue_next_turn("retry smaller")  [retry]
  │     **任何 phase 都允许入队**（Pi: 664-667）       │
  │                                                    │
  │  ===  client → session/cancel  ===================│
  │                                                    │
  │  ── abort path 开始 ──                            │
  │  cleared_steer = steer; steer = []                 │
  │  cleared_follow_up = follow_up; follow_up = []     │
  │  next_turn 保持不动                  [retry] ✓     │
  │                                                    │
  │  ActiveTurn.cancel.cancel()                        │
  │  ┌── provider stream fut: select!{                │
  │  │     _ = cancel.cancelled() => break,           │
  │  │     msg = stream.next()    => ...              │
  │  │   } ⇒ drop stream                              │
  │  ├── tool exec: cancel.clone() 检查/select! 同     │
  │  └── hook handlers：signal 传到 hook（§6）        │
  │                                                    │
  │  reverse-RPC 回收（Cancelled outcome 回收点）：    │
  │    for (id, sender) in pending_approvals.drain()  │
  │       sender.send(RequestPermissionResponse {     │
  │          outcome: Cancelled, meta: None,          │
  │       })  ── ACP 0.13 硬约束                       │
  │    pending_approvals 现在为空                      │ {}
  │
  │  await active_turn 任务退出（AbortOnDropHandle 兜底）
  │  phase → Idle
  │  emit events/session_aborted {
  │     cleared_steer, cleared_follow_up,
  │     next_turn_retained_count: 1                    [retry] ✓
  │  }
  │
phase = Idle                                       next_turn_queue
  │
  │  ◄── client: prompt("continue") → executeTurn
  │     splice next_turn 全部到 user message 前        []
  │     messages = [...drained, user_msg]
  │     phase → Turn
  │     ── new LLM stream req ──►
  │
```

**核心断言**：
- NextTurn 是**跨 abort 持久化**的队列（Pi 模式 agent-harness.ts:936-963，A3 §8）
- Pending `session/request_permission` **必须**在 abort 时用 ACP `Cancelled` outcome 显式回 client（不能让请求悬挂）；这是 zhive 与 Pi 的关键差：Pi 是 in-process `resolve(default)`，zhive 是跨进程 wire-level Cancelled

---

## 4. Pending reverse-request Map 的 lifecycle

### 4.1 类型 + ownership

```rust
// crates/zhive-core/src/state/turn.rs（B7 落地，本 deliverable 仅设计）

pub(crate) struct TurnState {
    /// 反向 RPC pending 队列。key = ACP request id，value = 应答 sink。
    /// 仅一个 owner：Engine dispatcher loop 持 `ActiveTurn` 的 Mutex。
    /// **不**跨线程 clone Arc —— 反向 RPC 完成或 cancel 都走 dispatcher
    /// 单点 mutate。
    pub pending_approvals: HashMap<
        RequestId,
        oneshot::Sender<RequestPermissionResponse>,
    >,
    // ... 其余字段见 B1 §6.1（granted_permissions / tool_calls / ...）
}
```

参考形态：tower-lsp `Pending(Arc<DashMap<Id, AbortHandle>>)` (`pending.rs:14-15`) —— 但 tower-lsp 用 `DashMap` 是因为跨任务并发访问；zhive 在 `TurnState` 内、由 dispatcher 串行 mutate，**不需要 DashMap**，普通 `HashMap` + 外层 `Mutex<ActiveTurn>` 已足够。

### 4.2 lifecycle 表

| 阶段 | 触发 | 操作 | 清理位点 |
|---|---|---|---|
| **创建** | tool_call 需要 permission，host 发 `session/request_permission` reverse-RPC | `pending_approvals.insert(req_id, oneshot::Sender)` | dispatcher loop |
| **正常完成** | client 回 `RequestPermissionResponse` | `pending_approvals.remove(req_id)?.send(response)?` | dispatcher loop（response 入站时） |
| **超时**（待 B6 决定 timeout 默认） | 内部 timer | `pending_approvals.remove(req_id)?.send(Selected{default_deny})` 或同 cancel 路径回 `Cancelled` —— 倾向后者保持语义一致 | dispatcher loop |
| **abort / cancel** | `session/cancel` notification | **遍历 drain**：`for (id, sender) in pending_approvals.drain() { sender.send(Cancelled outcome); }` —— ACP 0.13 硬约束 verbatim | abort handler（dispatcher 内串行执行） |
| **turn 自然结束**（无 pending 残留预期但要兜底） | turn_end event | `if !pending_approvals.is_empty() { warn!("leak"); for ... { send(Cancelled) } }` —— **fail-safe**，正常情况应为空 | turn_end handler |
| **engine shutdown** | `Engine.shutdown.cancel()` | 各 thread 的 ActiveTurn drop ⇒ `pending_approvals` drop ⇒ 各 `oneshot::Sender` drop ⇒ 接收端得 `RecvError`；但 wire-level 不发任何回包（ACP 不要求 shutdown 时发响应） | RAII（drop） |

### 4.3 fail-safe 设计点

1. **正常**情况：每个 insert 必有 remove 配对（dispatcher 串行保证）。
2. **异常**情况：B7 强制 turn_end / phase 切回 Idle 时检查 `pending_approvals.is_empty()`；非空则记 warn 并发 Cancelled。
3. **跨进程 wire 形态**：`oneshot::Sender` 的另一端不是 client，是 dispatcher 内的 reverse-RPC 应答 handler；handler 拿到 `RequestPermissionResponse` 后才走 outbound 写 JSON-RPC `result` 回 client。所以 `Cancelled outcome` 是**先 in-process oneshot 投递、再 outbound 序列化**两步。
4. **token leak 防御**：dispatcher loop 退出时用 `Drop` impl 保证 `pending_approvals.drain()` 都送 `Cancelled`。

> TODO(开放项 B7-1)：是否将 `pending_approvals` 升级到 `Arc<DashMap>` 以便允许 hook 异步消费？目前设计是 dispatcher 串行 mutate，**不允许 hook 直接访问**——所有交互通过 submission/event。倾向保持 HashMap+Mutex；DashMap 仅在出现"hook 想观察 pending 状态"用例时再加。

---

## 5. `PendingSessionWrites` buffer + flush 机制（zhive 版本）

### 5.1 采纳范围决策（关键问题 #5）

**决策**：**全面采纳，但语义对齐 EnginePhase 5 态而不是 Pi 5 态**。

| 维度 | Pi 现状（agent-harness.ts） | zhive 决策 |
|---|---|---|
| 触发入 buffer 的条件 | `phase !== "idle"`（5 态中除 idle 全入） | `phase != EnginePhase::Idle`（5 态中除 Idle 全入：Turn / Compaction / BranchSummary / Retry） |
| Buffer 项种类 | 8 种：`message / model_change / thinking_level_change / custom / custom_message / label / session_info / leaf` | 同语义。Rust 层 enum `PendingSessionWrite`，每 variant 对应一种 `Session::append_*` 方法 |
| Save point（flush 触发点） | 4 个：`prepareNextTurn` / `turn_end` / `agent_end` / `executeTurn finally` | 同 4 个 + **新增 1 个**：每次 `EnginePhase` 转回 `Idle` 时强制 flush（覆盖 compaction/branch_summary/retry 3 个非 turn phase 的退出路径） |
| Flush 失败的处理 | Pi: `await ...` 失败往上抛 | zhive: 失败先 emit `session_persistence_failed` event + 保留未 drain 项；不丢数据 |

**为什么"全面采纳"而不只 compaction phase 用**：
- Pi 实测 4 个 save point 中**每个**都依赖 buffer：`prepareNextTurn` 在 turn-内 inner loop 也调（agent-harness.ts:440），不限于 compaction
- "phase ≠ idle 入 buffer" 的核心目的是**避免 turn 中途写持久化引入 race**（一个 turn 还没结束时如果中途写盘，crash 恢复看到"半个 turn"）；compaction 只是其中一例
- 5 态都符合这个 race 风险（compaction/branch_summary 同样可能中途崩）

**为什么不只在 compaction 用**：会引入 phase-条件不一致——`appendMessage` 在 Turn phase 走"立即写"，在 Compaction phase 走"buffer"——同一 API 双语义反而更难推理。统一 "非 Idle 必入 buffer" 简单可证。

### 5.2 schema 草图

```rust
// crates/zhive-core/src/state/pending_writes.rs（B7 落地）

#[derive(Debug, Clone)]
pub enum PendingSessionWrite {
    /// Pi: { type: "message", message }
    Message { message: AgentMessage },
    /// Pi: { type: "model_change", provider, modelId }
    ModelChange { provider: ProviderId, model_id: ModelId },
    /// Pi: { type: "thinking_level_change", thinkingLevel }
    ThinkingLevelChange { thinking_level: ThinkingLevel },
    /// Pi: { type: "custom", customType, data }
    Custom { custom_type: String, data: serde_json::Value },
    /// Pi: { type: "custom_message", customType, content, display, details }
    CustomMessage { custom_type: String, content: String, display: Option<String>, details: Option<Value> },
    /// Pi: { type: "label", targetId, label }
    Label { target_id: EntryId, label: String },
    /// Pi: { type: "session_info", name }
    SessionInfo { name: Option<String> },
    /// Pi: { type: "leaf", targetId }
    Leaf { target_id: EntryId },
}

pub(crate) struct PendingSessionWrites {
    queue: VecDeque<PendingSessionWrite>,
}

impl PendingSessionWrites {
    /// 智能分发：调用方传 `current_phase`，由内部决定是否真入 buffer
    /// 还是直透 `session.append_*`。这样调用点不必重复 phase 判断。
    pub async fn push_or_apply<S: Session>(
        &mut self,
        current_phase: EnginePhase,
        write: PendingSessionWrite,
        session: &S,
    ) -> Result<(), SessionError> {
        if current_phase == EnginePhase::Idle {
            apply_write(session, write).await
        } else {
            self.queue.push_back(write);
            Ok(())
        }
    }

    /// Flush 全部，按入队顺序。失败时**保留未 drain 部分**（Pi 是 shift 一个写一个，
    /// 失败立即抛——本 deliverable 沿用 Pi 行为：失败抛错，已 drain 的不回填）。
    pub async fn flush<S: Session>(&mut self, session: &S) -> Result<usize, SessionError> {
        let mut count = 0;
        while let Some(write) = self.queue.front().cloned() {
            apply_write(session, write).await?;  // 失败立即 propagate
            self.queue.pop_front();
            count += 1;
        }
        Ok(count)
    }

    pub fn is_empty(&self) -> bool { self.queue.is_empty() }
    pub fn len(&self) -> usize { self.queue.len() }
}

async fn apply_write<S: Session>(session: &S, write: PendingSessionWrite) -> Result<(), SessionError> {
    match write {
        PendingSessionWrite::Message { message } => session.append_message(message).await,
        PendingSessionWrite::ModelChange { provider, model_id } => session.append_model_change(provider, model_id).await,
        PendingSessionWrite::ThinkingLevelChange { thinking_level } => session.append_thinking_level_change(thinking_level).await,
        PendingSessionWrite::Custom { custom_type, data } => session.append_custom_entry(custom_type, data).await,
        PendingSessionWrite::CustomMessage { custom_type, content, display, details } => session.append_custom_message(custom_type, content, display, details).await,
        PendingSessionWrite::Label { target_id, label } => session.append_label(target_id, label).await,
        PendingSessionWrite::SessionInfo { name } => session.append_session_name(name.unwrap_or_default()).await,
        PendingSessionWrite::Leaf { target_id } => session.set_leaf_id(target_id).await,
    }
}
```

### 5.3 5 个 save point（zhive 版本）

| # | 触发位点（zhive engine 代码） | 对应 Pi 锚点 | flush 后副作用 |
|---|---|---|---|
| 1 | inner-loop tick 准备下一次 LLM 请求前（`prepare_next_turn`） | agent-harness.ts:440 | 无特殊事件（hot path） |
| 2 | `turn_end` event handler 内 | agent-harness.ts:497-499 | emit `EngineEvent::SavePoint { had_pending_mutations }` |
| 3 | `agent_end` event handler 内（agent loop 正常退出） | agent-harness.ts:503 | 切 phase 到 Idle（在 flush 之后） |
| 4 | `execute_turn` 的 finally 块（错误兜底） | agent-harness.ts:594-600 | 无 |
| 5 | **zhive 新增**：任何 phase → Idle 转换前（覆盖 compaction/branch_summary/retry 退出） | 无 Pi 直接对应；Pi 这些 phase 退出靠 finally + 0 buffer 写入因 phase 自身不调 appendXxx | emit `EngineEvent::SavePoint { had_pending_mutations }` |

### 5.4 与 cancel 的交互

- **abort 路径**不 flush —— 强 abort 意味"丢弃这个 turn 的所有未持久化变更"。Pi 行为同（`runAbortController.abort()` 不触发 flush，靠 `executeTurn finally` 兜底，但 finally 在 abort 路径上仍然 throw 不一定能写）。
- zhive 决策：abort 路径**保留 buffer 内容**，下次 phase 回 Idle 时通过 save point #5 flush（这等于"延迟提交"）。
- **fail-safe**：如果 buffer 在 engine drop 时仍非空，emit `SessionDataLoss { count, items }` event 通知 client；不静默丢失。

> TODO(开放项 B7-2)：abort 时是否提供"立即 flush 已 buffer 项目"开关？倾向**否**（保持语义清晰：abort = 丢；正常退出 = flush）。但 IDE 场景下用户可能期望 abort 也保住部分变更——待 B4（server transport）确认 UX 要求。

---

## 6. Hook signature 决定（带不带 `signal`）

### 6.1 决策

**Hook handler 签名（Rust 端）**：

```rust
pub type HookHandler<E, Ctx> = Arc<
    dyn for<'a> Fn(
        &'a E,
        &'a Ctx,
        Option<&'a CancellationToken>,
    ) -> BoxFuture<'a, Result<HookOutput, HookError>>
    + Send + Sync,
>;
```

**第 3 参 `Option<&CancellationToken>` —— 非必填**，与 Pi `signal?: AbortSignal` 对齐（hooks.md:21-32）。

### 6.2 为什么 Option

| 维度 | `&CancellationToken` 必填 | `Option<&CancellationToken>` |
|---|---|---|
| 简单 observer hook | 强制忽略参数 | 可省略，签名干净 |
| `compaction` / `branch_summary` 在 Pi 无主 abort token 注入 | 必须造假 token 传 | 直接传 `None` 或专属 token |
| 与 Pi 行为一致 | 偏离 | 对齐（Pi `signal?` 可选） |
| 关键长操作（compaction 真正 abort 需求） | 同 | 通过专属字段 `compaction_cancel` 显式注入 `Some(&...)`，不靠 throwaway token |

### 6.3 怎么传（按 emit 路径分）

| 触发位点 | 第 3 参传入 |
|---|---|
| Turn 内 hook（pre_tool_use / post_tool_use / ...）| `Some(&active_turn.cancel)` |
| `pre_compact` / `post_compact` hook | `Some(&engine.compaction_cancel)` ── **zhive 改进**：相比 Pi 用 `new AbortController().signal` 一次性 token（永远 not aborted），zhive 用引擎级 `compaction_cancel` 暴露 `cancel_compaction()` API + 父 `shutdown` 自动传播 |
| `pre_branch_summary` / `post_branch_summary` hook | `Some(&engine.branch_summary_cancel)` —— 同上 |
| Pure observer hook（如 metrics / tracing） | 可传 `None`（无副作用，不需要响应 abort） |
| Subagent 内 hook | `Some(&subagent.active_turn.cancel)` —— 子 turn 的 cancel token |

### 6.4 非 abort-aware hook 的兼容（关键问题 #6 核心）

**Pi 的兼容方式（hooks.md:21-32）**：用户写 `(event, ctx) => {...}`（不接 signal），TypeScript 允许多余参数被忽略，调用 `handler(event, ctx, signal)` 时第 3 参就丢了。

**Rust 类型不支持"自动 arity 收缩"**，必须靠 Option/泛型解决。**zhive 采取分层 trait + blanket adapter**：

```rust
// 公开给用户的 trait（无 signal 也行）
pub trait Hook<E, Ctx>: Send + Sync {
    fn call<'a>(&'a self, event: &'a E, ctx: &'a Ctx) -> BoxFuture<'a, Result<HookOutput, HookError>>;
}

// 想响应 cancel 的用户实现这个 trait
pub trait CancellableHook<E, Ctx>: Send + Sync {
    fn call<'a>(
        &'a self,
        event: &'a E,
        ctx: &'a Ctx,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<HookOutput, HookError>>;
}

// Engine 内部统一持有 cancellable 形态；blanket impl 把非 cancellable 包成 cancellable
impl<H, E, Ctx> CancellableHook<E, Ctx> for HookAdapter<H>
where H: Hook<E, Ctx>
{
    fn call<'a>(&'a self, e: &'a E, c: &'a Ctx, _cancel: &'a CancellationToken)
        -> BoxFuture<'a, Result<HookOutput, HookError>>
    {
        self.0.call(e, c)  // 忽略 cancel，直跑
    }
}
```

**含义**：
- 用户**默认**写 `impl Hook` —— 不需要管 cancel
- 长操作 hook（compaction / branch_summary / 自定义重 IO hook）实现 `CancellableHook`，Engine 在 emit 时把当前阶段的 token 注入
- Engine 内部签名永远是 `CancellableHook`，通过 blanket adapter 把简单 `Hook` 抬升
- **关键**：非 abort-aware hook **不能响应 cancel**，但只要它本身是短操作（< 100ms 典型），cancel 信号会在外层 dispatcher select! 阻断；hook 跑完后才发现"哦 turn 已经 cancel 了"，无副作用（返回值被丢弃）

### 6.5 失败模式 + 兜底

| 场景 | Engine 处理 |
|---|---|
| hook 是非 cancellable + 跑得很久（30s+）+ 中途 abort | hook 继续跑完；返回值被 Engine 丢弃（外层已切 phase=Idle） |
| hook 是 cancellable + 中途 abort | hook 自己 `select!{ _ = cancel.cancelled() => return Err(Cancelled), ... }` 主动退出 |
| hook 偷偷阻塞了主线程（同步 IO） | tracing log warn；Engine 不强杀（Rust 无 abort thread API） |
| hook panics | `catch_unwind` 兜底（B5 hook host 落地，本 deliverable 不展开） |

---

## 7. "Steer 不撤销当前 tool_call" 的对外文档措辞（可挪到 user guide）

### 7.1 段落 A（精炼，3-5 句版）

> **Steering does not interrupt the current tool call.** When you send a steer message while the agent is mid-turn, the message is queued and injected into the **next** LLM request within the same turn. Any tool call already in flight (file write, network request, subprocess) continues to completion. If you need to abort the current tool execution, use `session/cancel` instead — this will cancel the turn, return any queued `steer` / `followUp` messages to you via the `events/session_aborted` notification, and respond to all pending permission requests with the `Cancelled` outcome (per ACP).

### 7.2 段落 B（长版本，含 nextTurn 区别）

> zhive distinguishes three injection channels:
>
> - **`steer`** — queued while a turn is in progress; injected into the next LLM request **within the same turn**. The current tool call (if any) is **not** interrupted; the steering message will only influence subsequent model decisions. Cleared by `session/cancel`.
> - **`followUp`** — queued while a turn is in progress; injected when the agent would otherwise end the turn, continuing the inner loop instead. Also cleared by `session/cancel`.
> - **`nextTurn`** — queued at any time (including during a turn and **after** a cancel); injected into the **next** `session/prompt` call, prepended before the user message. **Preserved across `session/cancel`**, so you can stage follow-up work even after aborting.
>
> Rationale: cancelling an in-flight tool call mid-execution would not undo its side effects (a file write or HTTP POST cannot be rolled back). If true reversibility is required, the agent must implement compensating actions explicitly — `session/cancel` only stops further work, it does not undo work already begun.

### 7.3 一行 changelog 形态

> `session/steer`: queued and injected at the next LLM boundary within the current turn; never interrupts in-flight tool calls. Use `session/cancel` to stop the turn.

---

## 8. 关键问题逐条作答

### Q1. `CancellationToken` 来源
**答**：`tokio_util::sync::CancellationToken`。已在 workspace 依赖（`Cargo.toml` workspace.dependencies `tokio-util = { version = "0.7", default-features = false }`，第 45 行）。不需要新增 dependency；不需要 enable 额外 feature（`sync::CancellationToken` 在 default 内）。B1 §6.1 已采用同选型（`shutdown / ThreadHandle.cancel / ActiveTurn.cancel` 三层全部 `CancellationToken`）。

### Q2. 取消信号传播树
**答**：见 §2.1 总图。
- **`CancellationToken` clone**：Engine.shutdown → ThreadHandle.cancel（`.child_token()`）→ ActiveTurn.cancel（`.child_token()`）→ Provider/ToolCall/Hook handlers/Subagent（`.clone()`，cheap Arc）
- **独立有名字段**（不挂主 abort 树）：`Engine.compaction_cancel` / `Engine.branch_summary_cancel`，根仅 `shutdown` 父，**不是** active turn 子（compact/navigateTree 要求 phase=Idle，无 active turn）
- **channel 而非 token**：submission mpsc / event broadcast / pending_approvals HashMap<oneshot> —— 这些是数据流不是 cancel 流，cancel 期间靠 dispatcher 显式 mutate（drain pending_approvals 并 send Cancelled）

### Q3. Steer 不撤销 in-flight tool_call —— 时序图
**答**：见 §3.1 全图。核心断言：steer 入队时，in-flight tool_call **继续跑到完成**；steer 内容在 inner-loop 下一轮 LLM 请求之前 drain 注入 messages。Pi 锚点：agent-loop.ts:253 `getSteeringMessages()` 在每次 stream 启动前调用，但 tool 本体由 `runAgentLoop` 内的 `pendingMessages` 维持，不依赖 steer 是否插入。A3 §6.2 已固化。

### Q4. Reverse-request 回收（pending Map 清理）的 ownership
**答**：见 §4 全节。
- **Owner**：dispatcher loop（单一 mutator），通过 `Mutex<ActiveTurn>` 包住 `pending_approvals: HashMap`
- **清理时机**：
  - 正常 response 入站 → `remove + send`（dispatcher 内）
  - abort/cancel → `drain` 遍历送 `Cancelled outcome`（dispatcher 内，abort handler）
  - turn_end → 检查 `is_empty()`，非空记 warn + drain 送 Cancelled（**fail-safe**）
  - engine shutdown → RAII drop（不发任何 wire 回包，ACP 不要求 shutdown 时回）
- 不上 `Arc<DashMap>`：dispatcher 单点 mutate 已够；并发访问无需求

### Q5. `PendingSessionWrites` zhive 是否全面采纳
**答**：**全面采纳，但 5 个 save point 而非 4 个**（多 1 个：任何 phase → Idle 转换前）。详见 §5。理由：
- 4 个原 save point 实测每个都依赖 buffer（不只 compaction）
- "phase ≠ idle 入 buffer" 核心目的是**防 turn 半成品落盘** —— 5 态全适用
- 双语义（phase A 直写 / phase B buffer）反而难推理
- 仅在 compaction 用会出现 `appendMessage` 在 Turn phase 直写 / Compaction phase buffer，破坏 API 一致性

### Q6. Hook signature `signal: Option<&CancellationToken>` 是否必填
**答**：**Option（非必填）**，详见 §6。具体：
- 用户 trait `Hook<E, Ctx>` **无 signal 参**（默认简单形态）
- `CancellableHook<E, Ctx>` **第 3 参 `&CancellationToken`**（需要响应 cancel 的 hook 实现）
- blanket adapter 把 `Hook` 自动包成 `CancellableHook`（忽略 cancel）
- Engine 统一持有 `CancellableHook`，对**所有** emit 强注入 token：Turn phase 注入 `&active_turn.cancel`；compaction 注入 `&engine.compaction_cancel`；branch_summary 注入 `&engine.branch_summary_cancel`；observer-only hook 也收到 token 但忽略
- **关键差**：相比 Pi 的 `new AbortController().signal` 一次性 token（永远 not aborted，hook 收到等于 None），zhive 用有名 Engine 字段，**真正可被外部 cancel**（暴露 `cancel_compaction()` / `cancel_branch_summary()` API）

---

## 9. 未决项

> TODO(开放项 B7-1)：`pending_approvals` 是否升级到 `Arc<DashMap>`。目前 dispatcher 串行 mutate，不需要；若未来 hook 需直接观察 pending 队列状态再考虑。

> TODO(开放项 B7-2)：abort 路径是否提供"立即 flush 已 buffer 项目"开关。倾向**否**（abort = 丢，flush 由下次 phase 回 Idle 触发）；IDE UX 待 B4 确认是否需要。

> TODO(开放项 B7-3)：compaction / branch_summary 的 cancel 公开 API 形态。是 `cancel_compaction(reason: String)` 单独 RPC，还是复用 `session/cancel` 并通过 phase 区分？前者更显式但增 RPC method；后者复用但 client 难知道当前在哪个 phase。倾向前者，B4 server transport 决。

> TODO(开放项 B7-4)：`PendingSessionWrites::flush` 失败时**已 drain 部分**是否回填到 buffer 头。Pi 行为是不回填（失败抛错，已 shift 的丢失）；zhive 倾向沿 Pi 但要 emit `partial_flush_failure` event 让 client 决策。

> TODO(开放项 B7-5)：`Engine.compaction_cancel` 与 `Engine.branch_summary_cancel` 是 per-engine 单例还是 per-call 重建？Pi 是 per-call `new AbortController()`。zhive 倾向 **per-call 重建**（避免 stale state），但需要 `Engine::start_compaction()` 入口构造新 token 并存入 `Mutex<Option<CancellationToken>>` 字段供 `cancel_compaction()` 取用。

> TODO(开放项 B7-6)：`events/session_aborted` notification 中 `cleared_steer / cleared_follow_up` 内容是否含完整 Item payload 还是只 ID？A3 §8 草图含完整 payload；wire 大小可能爆（用户连续 steer 100 条）—— 倾向仅返回前 N 条 + 总数，B4 决定。

> TODO(开放项 B7-7)：非 abort-aware hook 长时间（30s+）阻塞时是否记 warning。tracing log 倾向 emit warn，但 threshold 暂占位 5s；B5 hook host 落定。

---

## 10. 与 A3 / B1 deliverable 的衔接核对

| 项 | A3 决策 | B1 决策 | B7 实现 |
|---|---|---|---|
| 三队列结构 | 定义 schema（§4.1）+ wire 形态 | — | 复用 `InjectionQueues` schema；时序由 dispatcher 串行驱动 §3 |
| `QueueMode` | `All / OneAtATime`（A3 §4.1） | — | drain 行为见 §3 时序 |
| abort 清理范围 | 清 steer/followUp、保留 nextTurn（A3 §6.2 + §8.1） | `ActiveTurn.cancel.cancel()` + `pending_approvals` drain | §3.3 + §4 |
| `Cancelled outcome` 回收 | 必填 ACP wire 响应（A3 §6.3） | `pending_approvals` HashMap（B1 §6.1） | §4 lifecycle 表 |
| CancellationToken 选型 | — | `tokio_util::sync::CancellationToken`（B1 §6.1） | §2.2 token 选型表沿用 |
| Engine phase 枚举 | — | 5 态（B1 §2.1） | §5.1 phase→Idle 转换是新 save point |
| broadcast 1024 / mpsc 容量 | — | B1 §6.2 表 | 本 deliverable 不动 |
| Hook 14 事件 schema | A4 owns | — | §6 仅决定 signature；事件枚举由 A4 |
| Subagent 继承 | A3 §7 | — | §2.1 subagent 子树 cancel token 父→子 |

---

## 11. 草图编译说明

本 deliverable 的 Rust 代码块**未提交到 crates/**。生产代码落地点：

- `crates/zhive-core/src/state/pending_writes.rs`（§5.2 `PendingSessionWrites` 实现）
- `crates/zhive-core/src/state/turn.rs`（§4.1 `TurnState.pending_approvals` 字段，已在 B1 §6.1 列出）
- `crates/zhive-core/src/state/queues.rs`（A3 §4.1 已草图，本 deliverable 复用）
- `crates/zhive-core/src/engine.rs`（compaction_cancel / branch_summary_cancel 字段）
- `crates/zhive-core/src/hook/host.rs`（§6.4 `Hook` / `CancellableHook` 双 trait + blanket adapter，B5 hook host 落地）

依赖：`tokio` (sync) / `tokio-util` (CancellationToken) / `serde` —— 均已在 workspace.dependencies。
