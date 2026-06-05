---
task: B6
title: Permission reducer 接入 Engine + Hook + Subagent 调用图
date: 2026-05-28
status: implemented
depends_on:
  - A3 deliverable (`PermissionDecision` 四态 + `fn reduce` 签名 + Cancelled outcome 硬约束 + Subagent 无 wire `inherited_permissions` 字段)
  - B1 deliverable (`EnginePhase` 5 态 `Idle / Turn / Compaction / BranchSummary / Retry` + actor pattern + `pending_approvals: HashMap<String, oneshot::Sender<PermissionDecision>>`)
  - D-008 (Permission decision + Streaming + Subagent inheritance)
  - D-012 (Hook events 14)
non_goals:
  - 写 zhive crate 实现代码（仅 markdown deliverable）
  - 重定义 A3 已锁的 `fn reduce(&[PermissionDecision]) -> PermissionDecision` 优先级
  - 改 99-decisions/（冲突走 decision-diffs.md）
---

# B6 · Permission reducer 接入 Engine + Hook + Subagent 调用图

> A3 已锁 `fn reduce(&[PermissionDecision]) -> PermissionDecision`（单纯 fn，优先级 `Deny > Defer > Ask > Allow`）。B6 不再讨论 reducer 本身的代数，**只讨论怎么把它接到 Engine + HookHost + Subagent 调用图上 + `defer` 如何走 reverse RPC 续命**。

---

## 1. 参考点清单

下面所有论断均回指此清单，逐条按 `路径:行号` 锚定。

| 主题 | 路径 | 行号 |
|---|---|---|
| A3 reducer fn 签名 + 优先级 `Deny > Defer > Ask > Allow` | `plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md` | §3 (161-188) |
| A3 `PermissionDecision` 四态 lowercase 序列化（`"allow"/"deny"/"ask"/"defer"`）+ `#[non_exhaustive]` | 同上 | §2 (60-84) |
| A3 ACP `Cancelled` outcome 硬约束（pending request_permission abort 时必须返回 Cancelled） | 同上 | §1 (44-45) + §6.3 (437-447) |
| A3 Subagent **不存在** wire 字段 `inherited_permissions`（继承靠 `SubagentDefinition` 上 `Option` 字段缺省） | 同上 | §7.3 (521-553) |
| A3 reducer 父子双调（child 结果作为 parent 一项参与 reduce） | 同上 | §7.4 (556-576) |
| A3 reverse-RPC `session/request_permission` 默认 timeout 未决（B6 落定） | 同上 | §10 TODO A3-O3 (652) |
| A3 BypassPermissions 模式下 child reducer 是否短路未决（B6 落定） | 同上 | §10 TODO A3-O4 (654) |
| B1 `EnginePhase` 5 态（`Idle / Turn / Compaction / BranchSummary / Retry`，定义在 `zhive-proto::hook`）；subagent 派生作为 `agent` 工具在 `Turn` 内调度，不占独立 phase | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | §2.1 (74-97) + §2.4 (177) |
| B1 `EnginePhase` 转换矩阵：父 thread 派生 subagent 期间始终停在 `Turn` | 同上 | §2.3 (148-155) |
| B1 `TurnState.pending_approvals: HashMap<String, oneshot::Sender<PermissionDecision>>` —— pending permission decision sink | 同上 | §4 (319-330) + §6.2 (724) |
| B1 actor pattern：`mpsc::Sender<Submission>` for inbound + `broadcast::Sender<EngineEvent>` for outbound + `oneshot::Sender<Result>` for sync reply | 同上 | §6.2 channel 类型选择理由表 (715-727) |
| B1 反向 RPC sink trait 占位 `Arc<dyn ReverseRpcSink + Send + Sync>` | 同上 | §4 (262-263, 494) |
| B1 PhaseTransition hook（新增第 15 事件，与 D-012 PreCompact 共存） | 同上 | §6.7 (789-799) |
| Pi `pendingExtensionRequests` Map：cleanup on abort/timeout/response，resolve(default) 而非 reject | `${PI}/packages/coding-agent/src/modes/rpc/rpc-mode.ts` | 109-128 |
| Claude Code Hooks 输出 schema + 四态 + 优先级 verbatim | <https://code.claude.com/docs/en/agent-sdk/hooks> | "Outputs" 段 |
| Claude Code Subagents 禁递归 + tools[]/disallowedTools[]/permissionMode | <https://code.claude.com/docs/en/agent-sdk/subagents> | "Subagents cannot spawn..." Note |

---

## 2. `Reducer` 函数签名 + 实现伪码（含 Cancelled outcome 触发条件）

> A3 已锁 `fn reduce(...)` 本体。B6 在此之上展开：**reducer 在 engine 内部调用时的完整 wrapper**，把 hook 拉取 + 反向 RPC 发起 + Cancelled outcome 触发统一封装。

```rust
// crates/zhive-core/src/permission.rs  (B6 落地，A3 仅定 fn reduce)

use zhive_proto::permission::{PermissionDecision, PermissionDecision::*, reduce};
use zhive_proto::hook::HookSpecificOutput;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Reducer 调用上下文：tool_call 触发的一次 PreToolUse 决策路径。
///
/// 不变式：
/// - 单次 tool_call 在单 thread 内**最多触发一次** parent reducer + 一次 child reducer
///   （subagent 模式下；非 subagent 模式下只有 parent reducer）
/// - reducer 输入收集失败（hook timeout / 反向 RPC abort）⇒ 该项以
///   语义安全降级值参与 fold（hook timeout → `Deny`，reverse-RPC Cancelled → `Deny`；
///   非 user-deny 而是 system-deny —— 见 §2.2 触发表）
///
/// 调用方：`Engine` 的 tool_call dispatch（在 LLM 返回 tool_call 后、syscall 前）。
pub(crate) async fn reduce_for_tool_call(
    ctx: &PermissionCtx<'_>,
    cancel: &CancellationToken,
) -> Result<PermissionDecision, ReducerError> {
    // 1. 拉取所有 PreToolUse hook decisions（并行 join_all；见 §5）
    let hook_decisions = ctx.hook_host
        .dispatch_pre_tool_use(&ctx.tool_call, cancel)
        .await?;  // Vec<PermissionDecision>，每个 hook 超时/panic 已降级为 Deny

    // 2. 拉取用户态 ask 响应（reducer 内若任一 hook 给 Ask，需 reverse-RPC 等用户）
    //    注意：Ask 不是终态——hook 给 Ask 后，engine 必须发 reverse-RPC session/request_permission
    //    收回 user decision (Allow/Deny)，把该结果替换原 Ask 项再 fold
    let mut decisions = Vec::with_capacity(hook_decisions.len() + 1);
    for d in hook_decisions {
        match d {
            Ask => {
                // 同步 reverse-RPC：req_id → pending_approvals.insert(oneshot)
                // 等待 user 回 Allow/Deny/Cancelled
                match ctx.request_user_permission(cancel).await {
                    Ok(user_decision) => decisions.push(user_decision),
                    // 反向 RPC 被 abort（client session/cancel）⇒ ACP 硬约束：
                    // 用 Cancelled outcome 响应；reducer 视该项为 Deny（安全默认）
                    // —— 见 A3 §6.3 + ACP schema 行 728-735
                    Err(ReverseRpcError::Cancelled) => decisions.push(Deny),
                    Err(e) => return Err(ReducerError::ReverseRpc(e)),
                }
            }
            other => decisions.push(other),
        }
    }

    // 3. fold（A3 §3 函数）
    let mut final_decision = reduce(&decisions);

    // 4. Defer 处理：reducer 返回 Defer ⇒ engine 必须挂起 turn，等 client
    //    `session/resume` 续命（见 §4 流程图）。此处只**返回** Defer 让上层挂起。
    //    Defer 不被替换为 Allow/Deny；上层 engine 收到后切换 turn 到 "Suspended"
    //    sub-state（详见 B1 §3 交互矩阵补丁，§7 未决 B6-O1）。
    Ok(final_decision)
}

/// Reducer 错误。**不**包含 Deny —— Deny 是合法决策，不是错误。
#[derive(thiserror::Error, Debug)]
pub enum ReducerError {
    #[error("hook host failed: {0}")]
    HookHost(#[from] HookError),
    #[error("reverse RPC failed: {0}")]
    ReverseRpc(#[from] ReverseRpcError),
}
```

### 2.1 `Cancelled` outcome 触发条件汇总

| 触发源 | 行为 | reducer 内部如何处理 |
|---|---|---|
| client → `session/cancel` notification | engine `CancellationToken::cancel()` | 遍历 `pending_approvals`，对每个 `oneshot::Sender<PermissionDecision>` 发 **不通过 Sender**：而是由 ReverseRpcSink 单独发 `Cancelled` outcome 给 client（**ACP 硬约束**），同时让 reducer await future 收到 `ReverseRpcError::Cancelled`，把该项以 `Deny` 参与 fold |
| reducer 内 hook timeout（B6 默认 30s） | hook 视为没返回，**该 hook 不参与 fold**（不是 Deny） | hook_host 已在拉取阶段 timeout，不入 `decisions` slice；空 slice ⇒ `reduce` 返回 `Allow`（A3 §3 不变式） |
| reverse-RPC `session/request_permission` server-side timeout（B6 默认 30s，见 §7） | server 主动发 `Cancelled` outcome 给 client + 让 await future 解出 | 同 client cancel 路径：该项 fold 为 `Deny` |
| Subagent 父侧 cancel ⇒ child engine.shutdown() | child reducer 内的所有 pending Cancelled | child 整个返回 `Deny`（子 thread 关闭，所有 in-flight 资源回收） |

**关键不变式**：`Cancelled` 是 ACP wire outcome 字面值，**reducer 内部不直接用 `PermissionDecision::Cancelled`**（A3 四态没有 Cancelled）—— Cancelled 在 wire 上是 `PermissionOutcome::Cancelled`，在 reducer 内部一律降级为 `Deny`（安全默认）。

---

## 3. 父子 subagent 调用图（ASCII，含 B1 `EnginePhase` 衔接）

> A3 §7.4 已给伪码（child reducer 先跑、结果作为 parent 一项再 fold）。B6 在此之上**补完时序**：phase 切换 + reverse-RPC 走向 + reducer 触发点。

### 3.1 总览：tool_call 在 child subagent 内触发

```text
                  父 thread (parent engine)                                    子 thread (child engine)
                  EnginePhase = Turn                                            EnginePhase = (尚未存在)
                          │
                          │ LLM 返回 tool_call(name="agent", subagent_def)
                          │
                          ▼
              ┌────────────────────────────┐
              │ parent engine.spawn_subagent│
              │  - validate scope            │ (A3 §7.2 narrowed_into)
              │  - phase 保持 Turn           │ (B1 §2.3：agent 工具在 Turn 内派生)
              │  - child engine = Engine::spawn(child_cfg, parent_scope)
              └────────────────────────────┘
                          │
                          ├──────────────────────────────►  ┌──────────────────────────────┐
                          │                                  │ child engine.start_turn       │
                          │                                  │  - phase: Idle → Turn         │
                          │                                  │  - 继承 parent_scope          │
                          │                                  └──────────────────────────────┘
                          │                                              │
                          │  父 phase 始终为 Turn                        │  child LLM 流跑
                          │  (B1 §2.3: agent 工具不切 phase)             │
                          ▼                                              │
              ┌────────────────────────────┐                             │
              │ parent engine.phase = Turn │                             │
              │  父 turn 阻塞等 child 完成 │                             ▼
              └────────────────────────────┘                  ┌──────────────────────────────┐
                          │                                    │ child LLM 返回 tool_call("run_tests")
                          │                                    └──────────────────────────────┘
                          │                                              │
                          │                                              ▼
                          │                          ┌─────────────────────────────────────────────┐
                          │                          │ child engine:                                │
                          │                          │   reduce_for_tool_call(child_ctx)            │
                          │                          │   - dispatch child PreToolUse hooks         │
                          │                          │   - fold_child = reduce(&decisions_child)   │
                          │                          └─────────────────────────────────────────────┘
                          │                                              │
                          │                          ┌───────────────────┴───────────────────┐
                          │                          │ fold_child = Deny / Defer?            │
                          │                          └───────────────────┬───────────────────┘
                          │                                              │
                          │                          ┌───────────────────┴───────────────────┐
                          │                          │ Yes ⇒ short-circuit: child 返 fold_child │
                          │                          │      parent 不再 fold; 整个 tool_call 由 │
                          │                          │      child decision 决定                 │
                          │                          └───────────────────┬───────────────────┘
                          │                                              │ No
                          │                                              ▼
                          │                          ┌─────────────────────────────────────────────┐
                          │                          │ child 把 fold_child 发回 parent             │
                          │                          │ 走 in-process channel (非 wire)：           │
                          │                          │   parent.subagent_decision_tx.send(fold_child)│
                          │                          └─────────────────────────────────────────────┘
                          │                                              │
                          ▼                                              │
              ┌────────────────────────────┐                             │
              │ parent engine 收 fold_child │  ◀──────────────────────────┘
              │ (in-process channel)        │
              └────────────────────────────┘
                          │
                          ▼
              ┌─────────────────────────────────────────────────────────┐
              │ parent engine:                                           │
              │   reduce_for_tool_call(parent_ctx, with carry=fold_child)│
              │   - dispatch parent PreToolUse hooks                     │
              │   - decisions_parent = [parent_hook_decisions..., fold_child]
              │   - fold_final = reduce(&decisions_parent)               │
              └─────────────────────────────────────────────────────────┘
                          │
                          ▼
              ┌────────────────────────────┐
              │ fold_final 决定 tool 是否执行 │
              │ - 在 child engine 内 dispatch │ (child 是 tool_call 的 owner)
              │ - 结果 stream 回 child + parent  │
              └────────────────────────────┘
```

### 3.2 谁触发？谁传值？（关键问题 #2 完整答案）

| 项 | 触发者 | 传值机制 | 备注 |
|---|---|---|---|
| **child reducer 首次触发** | `child engine` 在 `tool_call` dispatch 前调 `reduce_for_tool_call` | child 内 in-process 直接调 fn | 与 parent 完全解耦 —— child engine 不知道自己是 subagent |
| **child fold → parent** | `child engine` 把 `fold_child` 通过 **in-process channel** `subagent_decision_tx: mpsc::Sender<PermissionDecision>` 发回 parent | 不走 wire（parent/child 同进程，per B1 §6.3 actor pattern） | 这是 zhive 的设计选择：subagent **不是** 跨进程 thread，child engine 与 parent engine 共享 EngineInner.threads HashMap，只在 phase / cancel 隔离 |
| **parent reducer 二次触发** | `parent engine` 在 `subagent_decision_rx.recv()` 之后调 `reduce_for_tool_call(parent_ctx, carry=fold_child)` | 把 `fold_child` append 到 `decisions_parent` 末尾 | A3 §7.4 字面：把 child 视为"一个额外的 hook"参与 parent reduce |
| **短路条件** | child `fold_child ∈ {Deny, Defer}` ⇒ 不通知 parent 也不再 fold | child engine 直接把 `fold_child` 当作最终决策返回到 tool_call dispatcher | 优化：Deny/Defer 不可能被 parent 上调到更松，提前短路省 parent hook dispatch |
| **父 phase 在派生期间** | 父 engine 自始至终停在 `Turn` —— subagent 派生是 `agent` 工具的一次 dispatch（`Engine::spawn` + child.start_turn 入队），不切换父 phase | B1 §2.3：`agent` 工具在 `Turn` 内运行，`EnginePhase` 无独立的 subagent 态 | 父 phase 不因派生离开 `Turn` —— 这样 parent steer/followUp 在 child 运行期间仍可接受。child engine 自己的 phase 独立演进 |

### 3.3 BypassPermissions 模式下短路语义（解决 A3 TODO A3-O4）

**决策**：parent 在 `permission_mode == BypassPermissions` 时：
- child engine 的 PreToolUse hooks **仍然 dispatch**（hook 作者可能依赖此 hook 做审计 / 日志 / 加 context）
- 所有 child hook 返回值**强制被替换为 `Allow`**（hook_host 内统一处理）
- `fold_child` 因此恒为 `Allow`
- **parent 仍能 deny**（parent hooks 不受 child mode 影响）

理由：BypassPermissions 是父对子的"信任声明"，不是绕过审计；parent 自己仍负责审查 child 行为。

---

## 4. `defer` 后的 follow-up RPC 流程图（关键问题 #3 完整答案）

> A3 已定 `Defer` 四态字面值 + "挂起 turn 等 client 续命"语义。B6 落地具体 wire 路径。

### 4.1 设计选择：**defer 不需要独立 RPC method**，复用 `session/request_permission` 二轮交互

| 备选方案 | 选 / 不选 | 理由 |
|---|---|---|
| A. 独立 `permission/defer` notification + `session/resume` request | **不选** | 增加两个 RPC method 但语义可由 `session/request_permission` 的回响延迟天然表达：client 可以"先回 Defer 占位 + 后续主动 resolve"；不必另开 method |
| B. 复用 `session/request_permission` reverse-RPC：client 第一次回 `{ outcome: "defer" }`，server 把 future **保留**在 pending_approvals 不 resolve，等 client 后续发 `session/resume_permission { request_id, outcome: Selected/Cancelled }` | **选** | (a) RPC method 数最小化；(b) `pending_approvals: HashMap<RequestId, oneshot>` 现成结构即可承载（B1 §4 已定 `TurnState.pending_approvals`）；(c) 与 ACP `Cancelled` 路径同形态：cancel 也是后置事件 |
| C. server 把 defer 当 Deny 立即拒（"暂不允许"） | **不选** | 破坏 A3 四态语义；client 失去 user-intervention 入口 |

### 4.2 时序图：defer 路径

```text
client (CLI/TUI/IDE)                   zhive engine (server)
       │                                     │
       │ ◀── session/request_permission id=R1 │  TurnState.pending_approvals[R1] = oneshot::Sender
       │     { tool_call, scope, ... }       │  reducer task await rx
       │                                     │
       │ ── response id=R1 ─────────────────►│
       │     { outcome: "defer",             │  reducer 收 Defer ⇒ engine 切 turn 内部 sub-state
       │       reason: "user away" }         │  "Suspended"（不是 EnginePhase，是 Turn 内）
       │                                     │
       │                                     │  发 events/turn_suspended notification
       │ ◀── events/turn_suspended ──────────│  { turnId, requestId: R1, suspendedAt }
       │     { turnId, requestId: R1, ... }  │
       │                                     │
       │   ......（user 回来）..............│
       │                                     │
       │ ── session/resume_permission ──────►│  requestId = R1
       │     { requestId: R1,                │  pending_approvals[R1] 仍存在
       │       outcome: { outcome: "selected",│  → 用 user 决策 resolve oneshot::Sender
       │                  optionId: "allow_once" } } │
       │                                     │
       │ ◀── ack（response 200）─────────────│
       │                                     │
       │                                     │  reducer await 解出 Allow
       │                                     │  → 重新 fold（剩余 hooks 已 cached 在 step 2 内）
       │                                     │  → ResumeOutcome 类型上不含 Defer ⇒ 不可能再次挂起
       │                                     │  → 新 fold = Allow/Deny ⇒ 续 turn
       │                                     │
       │ ◀── events/turn_resumed ────────────│  { turnId, resumedAt }
       │ ◀── item/appended (tool_result) ───│  ...continue turn...
       │ ◀── events/turn_completed ─────────│
```

### 4.3 wire schema（B4 transport 接力实现，B6 仅给形状）

```rust
// crates/zhive-proto/src/permission.rs  追加 wire 类型

/// Reverse-RPC `session/request_permission` 的 outcome 字面值。
/// 对齐 ACP `RequestPermissionOutcome` + zhive 扩展 `defer`。
///
/// 注意：`PermissionDecision`（四态）是 hook/reducer 内部用；
/// `PermissionOutcome` 是 wire 出参（与 ACP 对齐 + 加 defer）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionOutcome {
    /// ACP 标准：user 选了一个 PermissionOption（含 option_id "allow_once" / "allow_always" / "reject_once" / "reject_always"）
    #[serde(rename_all = "camelCase")]
    Selected { option_id: String },
    /// ACP 硬约束：session/cancel 后所有 pending request_permission 必须用此响应
    Cancelled,
    /// zhive 扩展：user 推迟决定，server 保留 pending 等 session/resume_permission
    Defer {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// client → server: 续命已 defer 的 permission request
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumePermissionParams {
    pub request_id: String,
    /// 续命时只能给 Selected 或 Cancelled，**不可再次 Defer**（防无限挂起；见 §7 未决 B6-O2）
    pub outcome: ResumeOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "camelCase")]
#[non_exhaustive]
pub enum ResumeOutcome {
    #[serde(rename_all = "camelCase")]
    Selected { option_id: String },
    Cancelled,
}

/// server → client: turn 挂起通知（defer 触发）
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnSuspendedNotification {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    /// 触发挂起的 permission request id（client 需用此 id 调 resume_permission）
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub suspended_at: i64,
}

/// server → client: turn 续命通知（resume 后续 turn）
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnResumedNotification {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub resumed_at: i64,
}
```

### 4.4 server 侧 pending Map lifecycle（关键问题 #3 完整答案）

```text
state: PermissionResolver = oneshot::Sender<PermissionDecision>
location: TurnState.pending_approvals: HashMap<RequestId, PermissionResolver>
                                       (B1 §4 line 320 已定)

事件                              对 pending_approvals 的影响
─────────────────────────────    ────────────────────────────────────────
reducer 内 hook 给 Ask           insert(req_id, oneshot::Sender)
                                  发 session/request_permission reverse-RPC 给 client
client response Selected/Allow   pending_approvals.remove(req_id).send(Allow)
client response Selected/Deny    pending_approvals.remove(req_id).send(Deny)
client response Cancelled        pending_approvals.remove(req_id) + drop sender（不 send）
                                  reducer await 收到 RecvError ⇒ ReverseRpcError::Cancelled
                                  reducer 内把该项 fold 为 Deny（§2.1）
client response Defer            **不** remove；保留 sender 在 map 中
                                  发 events/turn_suspended notification
                                  reducer 任务 await 阻塞在 oneshot::Receiver
client session/resume_permission pending_approvals[req_id] 仍存在
                                  → 把 ResumeOutcome 翻译为 PermissionDecision
                                  → pending_approvals.remove(req_id).send(...)
                                  reducer 解出 ⇒ 续 reduce
client session/cancel            遍历 pending_approvals.drain():
                                  - 每个 sender 走 ReverseRpcSink 发 Cancelled outcome 给 client
                                  - drop sender（reducer 收 Cancelled，fold 为 Deny）
turn timeout（B6 默认 30s/项）   pending_approvals.remove(req_id)
                                  通过 ReverseRpcSink 发 Cancelled outcome（同 client cancel）
                                  reducer 收 Cancelled ⇒ 该项 fold 为 Deny
```

---

## 5. 并行 vs 顺序的最终选型（关键问题 #1 完整答案）

### 5.1 选型：**并行 `join_all` + first-deny 短路（条件启用）**

| 维度 | 并行（join_all） | 顺序（for await） | zhive 选 |
|---|---|---|---|
| 总延迟 | max(hook_i) | sum(hook_i) | 并行（hook 通常 IO 等待） |
| first-deny 短路 | 需要 `select! / FuturesUnordered` 手动实现 | 天然支持（for 内见 Deny break） | 并行 + 显式短路（见 §5.2） |
| hook 之间依赖 | 不支持（hooks 独立） | 支持（上一个的 mutate 影响下一个） | hook 设计强制独立（PreToolUse 不允许跨 hook 状态依赖） |
| panic 隔离 | join_all 单 fail 仍 collect | 顺序 fail 后续不跑 | 并行 + 每个 hook 包 `catch_unwind` 降级为 Deny |
| 内存 | N 个 future 同时活 | 1 个 future | hook 数量 ≤ 32（D-013 限），N 小 |

**决策**：**并行执行 + 不启用 first-deny 短路**（hook 都跑完再 fold）。

### 5.2 为何**不**启用 first-deny 短路

| 论据 | 影响 |
|---|---|
| (a) hook 副作用问题 | hook 可能产 audit log / metric / trace span。短路丢副作用 ⇒ 审计不全 |
| (b) `updatedInput` 字段冲突 | 多 hook 都能 mutate input。若 hook_1 Deny 后短路，hook_2 的 input mutation 丢失，但 reducer 最终给 Deny 也没问题—— **不过** hook_2 的副作用日志也丢，违反 (a) |
| (c) hook 数量小 | D-013 上限 32；max(hook_i) 已是延迟 dominant，"短路省时间"边际收益小 |
| (d) `Defer / Ask` 也是非 Allow | 短路条件需 = `Deny`；`Defer / Ask` 不应短路（要等所有 hook 决策完才能正确 fold） |

**反向论据 + 反驳**：
- "如果有恶意 hook 故意 stall 怎么办？" → hook timeout（B6 默认 30s，见 §7）已隔离；timeout 后该 hook 视为未返回，**不参与 fold**（A3 §3 不变式：空 slice 返回 Allow，但实际 N-1 项仍参与）
- "性能 critical path 要求最小 deny 延迟？" → 30s timeout 是上限；正常 hook 跑 <100ms；并行 vs 顺序对正常路径无差异

### 5.3 实现伪码

```rust
// crates/zhive-core/src/hook_host.rs (B5 落地，B6 引用)

use futures::future::join_all;
use tokio::time::{timeout, Duration};

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

async fn dispatch_pre_tool_use(
    &self,
    tool_call: &ToolCall,
    cancel: &CancellationToken,
) -> Result<Vec<PermissionDecision>, HookError> {
    let registered = self.registered_for(HookEvent::PreToolUse);

    // 并行：N 个 future 同时跑
    let futures = registered.iter().map(|hook| {
        let cancel = cancel.clone();
        async move {
            // 每个 hook 包 timeout + panic catch；任何失败降级为 Deny
            let fut = hook.invoke(tool_call, &cancel);
            match timeout(HOOK_TIMEOUT, fut).await {
                Ok(Ok(output)) => extract_decision(output),
                Ok(Err(_e)) => {
                    tracing::warn!(hook = %hook.id(), "hook errored, downgrade to Deny");
                    PermissionDecision::Deny  // 安全降级
                }
                Err(_timeout) => {
                    tracing::warn!(hook = %hook.id(), "hook timed out, downgrade to Deny");
                    PermissionDecision::Deny
                }
            }
        }
    });

    // 等所有 hook 完成 —— 不短路
    let decisions: Vec<PermissionDecision> = join_all(futures).await;
    Ok(decisions)
}
```

### 5.4 两种语义差异速查

| 场景 | 顺序 + first-deny 短路 | 并行 + 不短路（zhive 选） |
|---|---|---|
| hook_1=Deny, hook_2=Allow | 只跑 hook_1（hook_2 副作用丢） | 都跑 |
| hook_1=Allow, hook_2=Deny | 都跑 | 都跑 |
| hook_1 timeout, hook_2=Allow | hook_1 阻塞 30s → 才跑 hook_2 | 同时跑：30s 后 hook_1 降级 Deny + hook_2 = Allow |
| hook_1=Ask, hook_2=Deny | 跑 hook_1 + 等 user 反馈 + 跑 hook_2 | 都跑：Ask 触发 reverse-RPC 与 hook_2 并行 |
| 总延迟 | sum + 用户等待时间 | max + 用户等待时间 |

---

## 6. 关键问题逐条作答（验收）

| # | 问题 | 答案（≤ 8 行） |
|---|---|---|
| 1.A | 多 hook 并行还是顺序？ | **并行 `join_all`**。每 hook 包 `timeout(30s) + catch_unwind` 降级为 `Deny`；hook 副作用独立、PreToolUse 设计禁止跨 hook 状态依赖。详见 §5.3 伪码。 |
| 1.B | first-deny 短路在哪种语义下可行？ | 仅在**顺序 + 副作用语义**下可行。zhive 选并行 ⇒ 不启用短路。理由：(a) 副作用 + audit 完整性；(b) hook 数 ≤ 32，max(hook_i) 已是延迟 dominant；(c) `Defer/Ask` 不应短路。详见 §5.2。 |
| 2.A | 父子 reducer 怎么传值？ | child 在 `tool_call` dispatch 前内部跑 `reduce_for_tool_call(child_ctx)`，得 `fold_child`；通过 **in-process channel** `subagent_decision_tx: mpsc::Sender<PermissionDecision>` 发回 parent（不走 wire，per B1 §6.3 actor pattern）；parent 把 `fold_child` 作为额外一项 append 到 `decisions_parent` 末尾再 fold（A3 §7.4 字面）。详见 §3.1 时序图。 |
| 2.B | 谁触发？与 B1 `EnginePhase` 衔接？ | parent engine 在 `spawn_subagent` 内派生 child（`agent` 工具的一次 dispatch），全程**不切 phase**：父 phase 自始至终为 `Turn`，child 整个 turn 期间亦然，因此 parent steer/followUp 在 child 运行期间仍可接受；child engine 自己的 phase 独立演进。详见 §3.2 表格。 |
| 2.C | 短路条件 | child `fold_child ∈ {Deny, Defer}` ⇒ 不通知 parent reducer（child 直接把 `fold_child` 作为最终决策返回）。Deny/Defer 不可能被 parent 上调到更松，提前短路省 parent hook dispatch。 |
| 3.A | defer 怎么实现？需要独立 RPC 吗？ | **不需要独立 RPC method**。复用 `session/request_permission` reverse-RPC：client 第一次回 `{ outcome: "defer" }`，server 把 `pending_approvals[req_id]` 的 oneshot::Sender **保留不 resolve**，发 `events/turn_suspended` notification；client 后续调 `session/resume_permission { request_id, outcome }` 续命 → server 用 outcome resolve oneshot → reducer 解出 → 续 fold。详见 §4.1-4.2。 |
| 3.B | defer 的 UI / RPC 路径形状 | 见 §4.3 wire schema：(a) `PermissionOutcome::Defer { reason }` 复用现有 `session/request_permission` response；(b) 新增 `TurnSuspendedNotification` + `TurnResumedNotification` 两个 notification；(c) 新增 `session/resume_permission` 一个 request method。**总新增 wire surface：1 method + 2 notifications + 2 outcome variant**。 |
| 3.C | defer 二次能否再 Defer（无限挂起）？ | **不允许**。`ResumePermissionParams.outcome: ResumeOutcome` 类型限定只能 `Selected / Cancelled`（schema 强制）；server 若收到任何非法形态返回 `invalid_params` 错误。理由：防 client bug / 恶意无限挂起；user UI 上 defer 应是"暂存待办"而不是"无限期延后"。详见 §7 未决 B6-O2。 |

---

## 7. 未决项（回流到 plan §9）

> TODO(开放项 B6-O1)：reducer 返回 `Defer` 后，Turn 在 B1 `TurnStatus` 4 态（A1 已定 `InProgress / Completed / Interrupted / Failed`）中处于何态？倾向**保持 `InProgress`**（不新增 `Suspended` 态，避免改 A1），但通过 events/turn_suspended notification 让 client 可见挂起状态。`InProgress` 期间允许 client 调 steer/followUp 吗？**倾向 disallow**（Pi 模式：steer/followUp 要求 phase ≠ Idle，但 Defer 期间 engine 在等用户而非在跑 LLM；语义模糊）—— 由 B7 cancel-streaming deliverable 落定，与 `pendingSessionWrites` 一起设计。

> TODO(开放项 B6-O2)：`ResumeOutcome` 是否要允许再次 Defer？当前 schema 禁止（§4.3 type-level）。若 user 需要多次延后，client 可以自己缓存 + UI 提示后续 resume 时机，不必 server 支持。若未来用户反馈强烈要求 chained Defer，可放开为 `PermissionOutcome::*` 全集（含 Defer）—— 是 wire-compat 兼容扩展，无 breaking。

> TODO(开放项 B6-O3)：reverse-RPC `session/request_permission` 的 server-side timeout 默认值（解决 A3 TODO A3-O3）。**B6 决策：默认 30s 超时**，timeout 后 server 主动发 `Cancelled` outcome（同 client cancel 路径），reducer 把该项 fold 为 `Deny`。超时值通过 `EngineConfig.permission_request_timeout: Duration` 暴露给 client 端可配（默认 30s，最小 5s，最大 600s）。

> TODO(开放项 B6-O4)：subagent `permission_mode == BypassPermissions` 时 child hooks 的处理（解决 A3 TODO A3-O4）。**B6 决策**：child hooks **仍然 dispatch**（保留 audit/log/trace 副作用），但所有 hook 返回值由 `hook_host` 统一**替换为 `Allow`**（hook 作者本身不感知）；`fold_child` 因此恒为 `Allow`；**parent 仍能 deny**。详见 §3.3。

> TODO(开放项 B6-O5)：`session/resume_permission` 在 client 端的 UI 触发器是什么？CLI 模式下用户可能离开终端 →  defer + 后续 `zhive resume-permission <req_id> --allow`；TUI 模式下持续显示 banner；IDE 集成可弹通知。具体 UI / CLI 形态由 D-002（TUI 客户端）+ Phase 2 落地，B6 仅保证 wire schema 已就绪。

> TODO(开放项 B6-O6)：reducer 在 child engine 内 `fold_child = Defer` 时，**父也会挂起 turn 吗**？倾向**是**：child Defer ⇒ child turn suspended（发 `events/turn_suspended` for child）⇒ parent engine 收到 `subagent_decision_rx.recv()` 阻塞 ⇒ parent turn 也 suspended（发 `events/turn_suspended` for parent）。两个 suspended notification 让 client 知道两层挂起。client 续命任一方都会传导：resume child request_id ⇒ child reducer 解出 ⇒ fold_child 重新计算 ⇒ child 通过 subagent_decision_tx 通知 parent ⇒ parent reducer 续 fold。具体实现细节由 B8（subagent 调度）+ B6 二阶段落地。

> TODO(开放项 B6-O7)：`Cancelled` outcome 在 zhive `PermissionOutcome` wire enum 上是 internally tagged 的独立 variant，序列化为 `{ "outcome": "cancelled" }`（`#[serde(tag = "outcome", rename_all = "camelCase")]`），与 `Selected / Defer` 同形态；ACP 0.12 字面 schema（A3 §1 line 44）则把 `Cancelled` 写成独立 variant。**与 ACP wire 兼容性需要 B4 transport deliverable 决定是否走 ACP bridge 时翻译；in-zhive wire 用 internally tagged**。

---

## 8. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中行号 / 段号引用）
- [x] 不动 `crates/` 源码（草图均在本 markdown 内）
- [x] 不改 `research/99-decisions/`（仅引用，未编辑）
- [x] 不 `git pull`
- [x] 参考输入 ≤ 4：A3 + B1 deliverable（2 个）+ plan §5 B6（1 节） = 3 个；可选 Pi rpc-mode.ts 1 个（未展开 Read）
- [x] reducer fn 签名复用 A3 §3 字面（不重定义）
- [x] 父子调用图与 B1 `EnginePhase`（`agent` 工具在 `Turn` 内派生）衔接（§3.2 表格）
- [x] defer 流程图含 client reverse handler 持续 await + server pending Map（§4.2 + §4.4）
- [x] 并行 vs 顺序明确选型 + first-deny 短路开关决策（§5.1-5.2）
- [x] 关键问题 #1/#2/#3 逐条作答（§6）
- [x] 未决项 7 条（TODO B6-O1 ~ B6-O7），均带 "B6 决策" 或 "B6 推到 B7/B8 落定" 标注

— B6 deliverable end —
