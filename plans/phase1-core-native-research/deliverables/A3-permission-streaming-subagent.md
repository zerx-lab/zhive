---
task: A3
plan: phase1-core-native-research
date: 2026-05-28
status: draft
depends:
  - research/99-decisions/README.md#d-008
  - research/99-decisions/README.md (R5 finding #1)
crate: zhive-proto (schema 落地点)
non-goals:
  - 写 zhive crate 源码
  - 改 research/99-decisions/（冲突走警告标记 + 走 decision-diffs.md 集中回流）
---

# A3 · Permission schema + StreamingBehavior + Subagent 继承

> **决策冲突警告：D-008 / R5 finding #1**
>
> D-008 当前声明 `StreamingBehavior: steer | followUp` **二元 mode**。本 deliverable 在通读 Pi 一手代码后建议 **修订为三队列模型** —— 与 Pi `agent-harness.ts:183-187` (`steerQueue` / `followUpQueue` / `nextTurnQueue`) 对齐：
> - 队列与 wire `streamingBehavior` 取值**不是 1:1**。Pi `streamingBehavior?: "steer" | "followUp"`（[rpc-types.ts:21](../../../../github/pi/packages/coding-agent/src/modes/rpc/rpc-types.ts)）只覆盖前两个；第三队列 `nextTurnQueue` 由独立的 `nextTurn` 命令驱动（[agent-harness.ts:664-667](../../../../github/pi/packages/agent/src/harness/agent-harness.ts)），**不进 `streamingBehavior` 枚举**
> - `abort()` 清前两队列、保留 nextTurn —— D-008 未规定 abort 与队列的交互
> - 每队列各自有 `QueueMode { All, OneAtATime }`（[types.ts:44](../../../../github/pi/packages/agent/src/harness/types.ts)），D-008 未提
>
> 建议词条修订草案见本文件末 `> 建议 D-008 词条修订`。本调研不直接改 99-decisions/，按 plan §10 走 `decision-diffs.md`。

---

## 1. 参考点清单

| 锚点 | 路径 | 行号 | 说明 |
|---|---|---|---|
| Pi 三队列字段声明 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 183-187 | `steerQueue / steeringQueueMode / followUpQueue / followUpQueueMode / nextTurnQueue` |
| Pi QueueMode | `${PI}/packages/agent/src/types.ts` | 44 | `export type QueueMode = "all" \| "one-at-a-time"` —— **注**：plan §4 A3 写 `harness/types.ts:44` 是错的，QueueMode 定义在上一级 `src/types.ts`，`harness/types.ts:2` 仅 import；同名变量 `steeringMode? / followUpMode?` 在 `harness/types.ts:811-812` |
| Pi steerQueue 注入时序（turn 内） | `${PI}/packages/agent/src/agent-loop.ts` | 167, 174-190, 253 | `getSteeringMessages?.()` 在 streaming 前 drain 注入 context.messages |
| Pi followUpQueue 注入时序（turn 后） | `${PI}/packages/agent/src/agent-loop.ts` | 256-261 | agent 本将 stop 时，drain followUp → 转 pendingMessages → 续 loop |
| Pi nextTurnQueue 注入时序（新 turn 开始） | `${PI}/packages/agent/src/harness/agent-harness.ts` | 533-541 | `executeTurn` 启动时 splice 全部 nextTurn 接到当前 user message 前 |
| Pi drain 回滚 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 391-401 | `queue.splice` → `emit on Err: queue.unshift(...)` |
| Pi `abort()` | `${PI}/packages/agent/src/harness/agent-harness.ts` | 936-963 | 清 steer/followUp、**不清 nextTurn**、发 `{ type: "abort", clearedSteer, clearedFollowUp }` |
| Pi `steer/followUp` 前置 phase 检查 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 652-662 | `if (this.phase === "idle") throw "invalid_state"` |
| Pi `nextTurn` 无前置 phase 检查 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 664-667 | 任何 phase 都可入队 |
| Pi `pendingSessionWrites` buffer | `${PI}/packages/agent/src/harness/agent-harness.ts` | 174, 459-481, 669-679 | phase ≠ idle 时 session 写入入 buffer，turn_end / agent_end flush |
| Pi wire `streamingBehavior` 二元 | `${PI}/packages/coding-agent/src/modes/rpc/rpc-types.ts` | 19-23 | `streamingBehavior?: "steer" \| "followUp"`；`nextTurn` 不在此枚举 |
| Pi reverse-RPC pending Map（前置技术） | `${PI}/packages/coding-agent/src/modes/rpc/rpc-mode.ts` | 109-128 | `pendingExtensionRequests` Map：cleanup on abort/timeout/response，**resolve(default) 而非 reject** |
| ACP `session/request_permission` | `${HOME}/.cargo/registry/.../agent-client-protocol-schema-0.12.0/src/client.rs` | 555-756 | `RequestPermissionRequest` / `PermissionOption` / `PermissionOptionKind { AllowOnce \| AllowAlways \| RejectOnce \| RejectAlways }` / `RequestPermissionOutcome { Cancelled, Selected { option_id } }` |
| ACP cancel + permission 交互硬约束 | 同上 | 727-735 | `Cancelled` outcome 在 `session/cancel` 后必须用于所有 pending request_permission |
| Claude Code Hooks 输出 schema | <https://code.claude.com/docs/en/agent-sdk/hooks> | "Outputs" 段 | `hookSpecificOutput.{ hookEventName, permissionDecision, permissionDecisionReason, updatedInput }` for PreToolUse；`{ additionalContext, updatedToolOutput }` for PostToolUse |
| Claude Code 四态 + 优先级 | 同上 | "Outputs" Note | `"allow" \| "deny" \| "ask" \| "defer"`；优先级 `deny > defer > ask > allow` |
| Claude Code Subagents | <https://code.claude.com/docs/en/agent-sdk/subagents> | "Subagents cannot spawn..." Note + `AgentDefinition` 表 | 禁递归；`tools[]` / `disallowedTools[]` / `permissionMode` 字段定义 |

---

## 2. `PermissionDecision / PermissionScope / StreamingBehavior` enum 草图

```rust
// crates/zhive-proto/src/permission.rs

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

/// Permission 四态决策。Wire 序列化对齐 Claude Code Agent SDK
/// (`hookSpecificOutput.permissionDecision`)，避免 N×M 翻译。
///
/// 优先级：`Deny > Defer > Ask > Allow`，见 [`reduce`] 实现。
///
/// 参考：Claude Code Hooks docs (verbatim)
/// > "deny takes priority over defer, which takes priority over ask,
/// >  which takes priority over allow."
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PermissionDecision {
    /// 拒绝。Reducer 中最高优先级，任一 hook 返回即终结。
    Deny,
    /// 推迟。挂起 turn，等 client `session.resume(turn_id)` 续命；
    /// `updatedInput` 字段在此态下**会被忽略**（Claude Code 行为）。
    Defer,
    /// 询问用户。触发 reverse-RPC `permission/request` → 等待响应。
    Ask,
    /// 允许。最低优先级。
    Allow,
}

/// Permission scope —— subagent 继承单位。父→子单向传递；
/// child 可缩窄不可放大（[`narrowed_into`] 验证）。
#[derive(
    Debug, Clone, PartialEq, Eq,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionScope {
    /// 工具白名单。`None` ⇒ 继承父全集；`Some(vec)` ⇒ 仅这些工具
    /// （对齐 Claude Code `AgentDefinition.tools[]`）。
    pub allowed_tools: Option<Vec<ToolName>>,
    /// 工具黑名单。父 `disallowedTools` 自动并入 child（对齐 SDK）。
    pub disallowed_tools: Vec<ToolName>,
    /// Subagent permission mode；`None` ⇒ 继承父 mode。
    pub permission_mode: Option<PermissionMode>,
    /// 子是否可以再 spawn subagent。
    /// Claude Code 硬约束：subagent 不能 spawn subagent，
    /// 因此子 scope 中此值**强制为 false**（[`narrowed_into`] enforce）。
    pub allow_subagent_spawn: bool,
}

/// 对齐 Claude Code `permissionMode`（含 `bypassPermissions`、`acceptEdits`
/// 等已知"安全雷区"模式 —— D-008 红线点）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionMode {
    /// 全部 tool 都走 reducer + reverse-RPC。
    Default,
    /// 编辑类工具自动 allow（read 仍走 reducer）。
    AcceptEdits,
    /// 全部自动 allow —— **父在此 mode 时，所有 subagent 强制继承**
    /// （Claude Code 安全雷区，D-008 已记录）。
    BypassPermissions,
    /// 测试态 / 默认 deny。
    Plan,
}

/// 三队列注入语义。**这是 in-process 状态机枚举，不是 wire-only enum**。
///
/// Wire 上仅 `Steer / FollowUp` 通过 `streamingBehavior?: "steer" | "followUp"`
/// 暴露（对齐 Pi `rpc-types.ts:21`）；`NextTurn` 由独立的 `session/next_turn`
/// RPC method 驱动，**不进 `streamingBehavior` 枚举**（避免与 Pi wire 字面冲突）。
///
/// > 决策冲突警告：D-008 写"二元 mode"，本枚举三态。详见本文件顶 警告块。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum StreamingBehavior {
    /// Turn 执行期间注入。下一次 LLM 请求前 drain → 注入 context.messages。
    /// **不撤销 in-flight tool_call**（Pi 模式）。
    Steer,
    /// Agent 即将 stop 时注入。drain → 续 loop 而不退出。
    FollowUp,
    /// 仅 idle 时入队、下一个 turn 启动时 splice 到 user_message 前。
    /// abort 不清此队列（**恢复 / 重发关键**）。
    NextTurn,
}
```

> TODO(开放项)：`ToolName` 类型在 A1 deliverable 决定（`String` newtype 还是 enum 复合？此处暂占位 `pub struct ToolName(pub String)`）。

---

## 3. Reducer 合并函数签名

**关键问题 #2 决策**：用**单纯 fn**（无状态、关联类型 0）+ in-process trait 仅在 `HookHost` 注册口暴露 `dyn Fn(...) -> PermissionDecision` 即可。**不**给 reducer 单独搞 trait。

```rust
/// Reducer：多个 hook 给出 decisions 后折叠为单一最终决策。
///
/// 不变式：
/// - 空 slice ⇒ `Allow`（无 hook 不阻塞）
/// - `deny` 出现一次即返回 `Deny`
/// - 其余按 `Defer > Ask > Allow` 取严
///
/// ```
/// use zhive_proto::permission::{reduce, PermissionDecision::*};
/// assert_eq!(reduce(&[]), Allow);
/// assert_eq!(reduce(&[Allow, Deny, Ask]), Deny);
/// assert_eq!(reduce(&[Ask, Allow, Defer]), Defer);
/// assert_eq!(reduce(&[Ask, Allow]), Ask);
/// ```
pub fn reduce(decisions: &[PermissionDecision]) -> PermissionDecision {
    use PermissionDecision::*;
    let mut best = Allow;
    for &d in decisions {
        best = match (best, d) {
            (Deny, _) | (_, Deny) => Deny,
            (Defer, _) | (_, Defer) => Defer,
            (Ask, _) | (_, Ask) => Ask,
            _ => Allow,
        };
    }
    best
}
```

**为什么选 fn 不选 trait**：

| 维度 | `fn reduce(&[Decision]) -> Decision` | `trait Reducer { fn reduce... }` |
|---|---|---|
| 状态 | 无 | 多余（reducer 无状态） |
| 调用点 | 直接 `reduce(&v)` | 需 `&self` / `Arc<dyn Reducer>` |
| 测试 | doctest 即覆盖 | 多一层 mock |
| 拓展（未来加 reducer 模式） | 加同名 fn 不破坏 | 加 trait method 是 breaking |

选 fn。需要可插拔时再加 trait —— 当前 0 个 use case 需要。

---

## 4. 三队列 + QueueMode 类型与状态机

### 4.1 类型草图

```rust
// crates/zhive-core/src/state/queues.rs  (B7 落地，A3 仅定 schema)

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::proto::Item;  // A1 决定

#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize, JsonSchema, Default,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum QueueMode {
    /// 一次 drain 全部
    All,
    /// 一次只 drain 一条（默认，对齐 Pi `steeringMode ?? "one-at-a-time"`)
    #[default]
    OneAtATime,
}

/// 三队列 holder。**不是 wire 暴露的类型**，仅 engine state。
/// Wire 暴露的是入队 / 出队 RPC（见 §7）。
#[derive(Debug, Default)]
pub struct InjectionQueues {
    pub steer: Vec<Item>,
    pub steer_mode: QueueMode,
    pub follow_up: Vec<Item>,
    pub follow_up_mode: QueueMode,
    pub next_turn: Vec<Item>,
    // next_turn 没有 mode：永远是 "全部 splice"（Pi: agent-harness.ts:534）
}

impl InjectionQueues {
    /// 失败回滚语义（Pi `drainQueuedMessages` 镜像）：
    /// 1. splice 出消息
    /// 2. 调 fallible op
    /// 3. on Err ⇒ `queue.splice(0..0, drained)` 还原顺序到队头
    ///
    /// **顺序还原**靠 `Vec::splice(0..0, drained)`：Pi 用 `unshift(...messages)`
    /// 是同语义（队头 push 多元素）。
    pub fn drain(&mut self, target: QueueTarget, mode: QueueMode) -> Vec<Item> {
        let q = self.queue_mut(target);
        match mode {
            QueueMode::All => std::mem::take(q),
            QueueMode::OneAtATime if !q.is_empty() => vec![q.remove(0)],
            QueueMode::OneAtATime => Vec::new(),
        }
    }

    pub fn restore_front(&mut self, target: QueueTarget, drained: Vec<Item>) {
        let q = self.queue_mut(target);
        q.splice(0..0, drained);
    }

    fn queue_mut(&mut self, t: QueueTarget) -> &mut Vec<Item> {
        match t {
            QueueTarget::Steer => &mut self.steer,
            QueueTarget::FollowUp => &mut self.follow_up,
            QueueTarget::NextTurn => &mut self.next_turn,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueTarget { Steer, FollowUp, NextTurn }
```

### 4.2 状态机：三队列 × Engine phase

| 事件 | Engine phase | 行为 |
|---|---|---|
| `enqueue_steer(msg)` | `Idle` | **拒绝**：`Error::InvalidState`（Pi: agent-harness.ts:653 同语义） |
| `enqueue_steer(msg)` | `Turn / Compaction / ...` | 入 `steer`，发 `queue_update` 事件 |
| `enqueue_follow_up(msg)` | `Idle` | **拒绝** |
| `enqueue_follow_up(msg)` | 非 `Idle` | 入 `follow_up` |
| `enqueue_next_turn(msg)` | **任何 phase** | 入 `next_turn`（Pi: agent-harness.ts:664-667 无前置） |
| drain `Steer` | 每次 inner-loop 顶 + turn_end 后 | 见 agent-loop.ts:167, 253 |
| drain `FollowUp` | inner-loop 退出后、agent_end 前 | 见 agent-loop.ts:256-261 |
| drain `NextTurn` | 新 `executeTurn(text)` 入口 | 见 agent-harness.ts:533-541 |
| `abort()` | 任意 | **清** steer + follow_up，**保留** next_turn；发 `clearedSteer / clearedFollowUp` |

> TODO(开放项)：`enqueue_next_turn` 在 `Idle` 入队场景下，是否需要主动通知 client "已入队但无 turn 在跑"？Pi 行为是静默入队 + 下次 `prompt()` 触发；zhive 是否要发 `next_turn_queued` 事件？

---

## 5. `HookSpecificOutput` struct 与 Claude Code 逐字段对齐表

```rust
// crates/zhive-proto/src/hook.rs

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::permission::PermissionDecision;
use serde_json::Value;

/// Hook 回调的完整输出。**字段名严格 camelCase 对齐 Claude Code SDK**。
///
/// 参考：<https://code.claude.com/docs/en/agent-sdk/hooks> "Outputs" 段。
#[derive(
    Debug, Clone, Default,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookOutput {
    /// Top-level：展示给用户的系统消息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Top-level：hook 后是否继续 agent loop。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#continue: Option<bool>,
    /// Async 模式开关（fire-and-forget）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#async: Option<bool>,
    /// Async 超时（ms），仅 `async=true` 有效。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<u64>,
    /// 事件特化输出（PreToolUse / PostToolUse / ...）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// 事件特化输出。**`tag = "hookEventName"` 是 Claude Code wire 字面要求**
/// （hookEventName 同时既是 discriminator 又是字段）。
#[derive(
    Debug, Clone,
    Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "hookEventName")]
#[non_exhaustive]
pub enum HookSpecificOutput {
    /// PreToolUse: 决定是否允许工具调用 + 可修改 input。
    #[serde(rename = "PreToolUse", rename_all = "camelCase")]
    PreToolUse {
        permission_decision: PermissionDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        permission_decision_reason: Option<String>,
        /// 修改后的 tool input。
        /// **重要**：`Defer` 态下此字段被忽略（Claude Code 行为）；
        /// `Allow / Ask` 才生效；mutate 后 host 必须 re-validate schema（红线 11）。
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
    },
    /// PostToolUse: 追加上下文 / 替换工具输出。
    #[serde(rename = "PostToolUse", rename_all = "camelCase")]
    PostToolUse {
        #[serde(skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_tool_output: Option<Value>,
    },
    /// 其他 12 事件由 A4 deliverable 落定。
    // ... (A4 owns)
}
```

### 5.1 逐字段对齐表

| Claude Code wire 字段 | zhive Rust 字段 | 备注 |
|---|---|---|
| `systemMessage` | `system_message` | serde `rename_all = "camelCase"` 自动 |
| `continue` | `r#continue` | Rust keyword escape |
| `async` | `r#async` | Rust keyword escape |
| `asyncTimeout` | `async_timeout` | |
| `hookSpecificOutput` | `hook_specific_output` | |
| `hookSpecificOutput.hookEventName` | `#[serde(tag = "hookEventName")]` | discriminator |
| `hookSpecificOutput.permissionDecision` | `permission_decision: PermissionDecision` | enum 序列化为 `"allow" / "deny" / "ask" / "defer"` |
| `hookSpecificOutput.permissionDecisionReason` | `permission_decision_reason: Option<String>` | |
| `hookSpecificOutput.updatedInput` | `updated_input: Option<Value>` | Pi 反例红线 11：mutate 后必须重验证 schema |
| `hookSpecificOutput.additionalContext` | `additional_context: Option<String>` | PostToolUse only |
| `hookSpecificOutput.updatedToolOutput` | `updated_tool_output: Option<Value>` | PostToolUse only |

**未对齐项**：无。本 deliverable 不引入 zhive-only 字段（避免提前发明 wire schema）。Pi `streamingBehavior` 字段单独走 `RpcCommand`，不是 `HookOutput` 一部分。

---

## 6. StreamingBehavior 取消状态机（ASCII 时序图）

### 6.1 总览：三条线 + 三队列 lifecycle

```
                ┌─────────────────────────────────────────────────────────┐
                │                  Engine phase = Turn                    │
                └─────────────────────────────────────────────────────────┘

t0  client → server : prompt("doStuff")                            (turn_start)
t1  engine          : drain steer (空) → spawn LLM stream req      ─┐
t2  LLM streams     : reasoning chunks → tool_call("run_tests")    │
t3  engine          : reverse-RPC permission/request → client      │  in-flight
                      pendingReverse[req_id] = (resolver, deadline)│  tool_call
t4  client          : enqueue_steer("also run lint") [phase=Turn] │  线
                      → steerQueue.push                            │
t5  client          : response permission/request → Allow          │
t6  engine          : tool exec begins (real syscall fired)        │
                                                                    │
    ===  here client sends `abort()` request  ============================
                                                                    │
t7  abort path      : clearedSteer = [...steer]; steer = []        │
                      clearedFollowUp = [...followUp]; followUp = []│
                      runAbortController.abort()  →  emits "abort"  │
t8  engine          : cancel_token signals                          │
                      ┌── in-flight tool_call: Pi 不主动撤(*)       │
                      │   ── 走 child-process kill 路径(*)          │
                      ├── pending reverse-request: emit Cancelled   │
                      │   outcome 到所有 pending（ACP 硬约束）       │
                      └── nextTurn 队列**保留**                     │
t9  engine          : phase → Idle; emit { type: "abort",          │
                      clearedSteer, clearedFollowUp }              ─┘
t10 client          : steer/followUp 现在被拒（phase=Idle）；
                      enqueue_next_turn("retry with smaller scope")
                      → next_turn.push（可在 Idle 时入队，无前置）
t11 client          : prompt("continue") → executeTurn() 入口
                      drains next_turn 全部 splice 到 user msg 前

  (*) 见下文 6.2 "Steer 不撤销 in-flight tool_call" 说明
```

### 6.2 关键决策（关键问题 #4）

| 维度 | Pi 行为 | zhive 决策 | 理由 |
|---|---|---|---|
| `Steer` 触发时 in-flight tool_call | **不撤销**，继续跑完；steer 消息在 LLM **下一轮**请求前才注入（agent-loop.ts:253） | 同 Pi | (a) 撤销 in-flight syscall 本来就脆（fs/network 已落副作用）；(b) 撤销 ≠ undo —— 留给 user via abort+rollback；(c) 与 Pi 对齐保留 wire-level 互操作潜力 |
| 已发的 reverse-request（permission/request）回收 | `pendingExtensionRequests` Map，AbortSignal 触发 `cleanup()` + **resolve(default)** 而非 reject（rpc-mode.ts:107-127） | 走 ACP `Cancelled` outcome（`RequestPermissionOutcome::Cancelled`），不 resolve(default) | ACP 0.12 硬约束（schema doc 行 728-735 verbatim）："client MUST respond to all pending session/request_permission requests with this Cancelled outcome"；Pi 是单进程内 default 兜底，zhive 是跨进程必须走 wire |
| Turn 边界是否重置 | `Steer` 不重置 turn；`FollowUp` 在 turn-end 后续 loop（伪 turn 复用）；`abort()` 强制 turn 边界关闭 | 同 Pi | turn = 一次 user input + 全 agent 响应（D-006）；steer 是 turn 内补丁，不破坏边界；abort 关闭边界并允许 nextTurn 续命 |
| `nextTurn` 在 abort 时是否清空 | **不清空**（agent-harness.ts:937-940 只清 steer/followUp） | 同 Pi | 这是 Pi 唯一保证"abort 后用户可重发未投递的消息"的机制，**核心语义**；D-008 未规定，本 deliverable 补完 |

### 6.3 单线特写：reverse-request 在 abort 时的 lifecycle

```
client                     server (zhive-core)
  │                              │
  │ <── permission/request id=A  │  pendingReverse[A] = (resolver, scope)
  │                              │
  │ ── session/cancel ──────────→│  abort_token.cancel()
  │                              │  for (id, r) in pendingReverse:
  │ <── response id=A ────────── │      r.resolve(Cancelled outcome)
  │     { outcome: "cancelled" } │  pendingReverse.clear()
  │                              │
```

zhive 在反向 RPC 侧持有 `HashMap<RequestId, PermissionResolver>`，cancel_token 触发时遍历 + resolve(Cancelled)。对比 Pi 的 in-process resolve(default)：zhive 跨进程 wire 必须显式发回 response（ACP 硬约束）。

> TODO(开放项)：`PermissionResolver` 的 timeout 策略 —— Pi 用 `setTimeout(..., timeout)` + `resolve(default)`。zhive 是否要在 server 侧也加 timeout？还是 client 自己负责？倾向 server 侧加默认 `Deny` timeout（30s？）—— B6 决定。

---

## 7. Subagent 继承规则的不变式

### 7.1 三大硬不变式

1. **父→子单向**：parent `PermissionScope` 必传给 child。child 在 spawn 时 **必填** `parent_scope: &PermissionScope` 参数，无 ambient 状态可读。
2. **子可缩窄不可放大**：child 的 `allowed_tools` 必须是 parent 的子集；`disallowed_tools` 必须是 parent 的超集；`permission_mode` 不能比 parent 更"宽"（`BypassPermissions > AcceptEdits > Default > Plan`）。违反 → spawn 时 `Error::ScopeWideningRejected`。
3. **Reducer 父子各执行一次**：单次 tool_call 触发 reducer **2 次**（child reducer first，结果作为 input 给 parent reducer，再 fold parent hooks 的 decisions）。

### 7.2 Rust 草图

```rust
impl PermissionScope {
    /// 验证 child scope 是否合法继承 self。**spawn 入口必调用**。
    pub fn narrowed_into(&self, child: &Self) -> Result<(), ScopeError> {
        // (1) allowed_tools 必须是子集
        if let (Some(parent_set), Some(child_set)) =
            (&self.allowed_tools, &child.allowed_tools)
        {
            for t in child_set {
                if !parent_set.contains(t) {
                    return Err(ScopeError::ToolNotInherited(t.clone()));
                }
            }
        } else if self.allowed_tools.is_some() && child.allowed_tools.is_none() {
            // child = None ⇒ 继承全集；但父非 None ⇒ child None 等价 widen，拒
            return Err(ScopeError::ChildMustExplicitlyNarrow);
        }

        // (2) disallowed_tools 必须是父超集
        for t in &self.disallowed_tools {
            if !child.disallowed_tools.contains(t) {
                return Err(ScopeError::DisallowedToolDropped(t.clone()));
            }
        }

        // (3) permission_mode 不能放大
        match (self.permission_mode, child.permission_mode) {
            (Some(p), Some(c)) if !mode_narrows(p, c) => {
                return Err(ScopeError::ModeWidened { parent: p, child: c });
            }
            _ => {}
        }

        // (4) 子不能 spawn 子（Claude Code 硬约束）
        if child.allow_subagent_spawn {
            return Err(ScopeError::RecursionForbidden);
        }
        Ok(())
    }
}

/// Returns true iff child mode is no broader than parent.
fn mode_narrows(parent: PermissionMode, child: PermissionMode) -> bool {
    use PermissionMode::*;
    fn rank(m: PermissionMode) -> u8 {
        match m {
            BypassPermissions => 3,
            AcceptEdits => 2,
            Default => 1,
            Plan => 0,
        }
    }
    rank(child) <= rank(parent)
}
```

### 7.3 Wire 形状（关键问题 #5）

**`Subagent.inherited_permissions` 字段在 wire 上长什么样**：

Subagent spawn 在 wire 上不是单独的 RPC（subagent 由 LLM 调 `Agent` tool 触发，wire 入口是 `tools/call`）。所以"继承"发生在 server 内部 spawn 时 —— **不必 wire 暴露**。

Wire 上需要的是 **`SubagentDefinition`**（client → server 发 settings 时传），结构如下：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentDefinition {
    /// 唯一名字（routing）
    pub name: String,
    pub description: String,
    pub prompt: String,
    /// None ⇒ 继承父 tools；Some ⇒ 仅此白名单（必须是父子集，server 端验证）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<ToolName>,
    /// None ⇒ 继承父 mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// 默认 false（Claude Code 硬约束）
    #[serde(default)]
    pub allow_subagent_spawn: bool,
    /// 其余字段 (model / mcpServers / maxTurns / ...) 由 B8 deliverable 决
}
```

**关键差异**：`inherited_permissions` 不作为字段存在 —— 继承是 server-side 行为，`SubagentDefinition` 上的 `Option` 字段语义即"None = 继承"。这与 Claude Code wire 完全对齐（无 `inherited_permissions` 字段，靠字段缺省）。

### 7.4 Reducer 双调（关键问题 reducer 父子两侧各执行一次）

```
tool_call inside child subagent
  │
  ├─ child engine runs PreToolUse hooks (child-scoped only)
  │    decisions_child = [hook1.ret, hook2.ret, ...]
  │    fold_child = reduce(&decisions_child)
  │
  ├─ if fold_child == Deny / Defer ⇒ short-circuit, propagate to parent as result
  │
  ├─ otherwise carry fold_child to parent
  │    parent engine runs PreToolUse hooks (parent-scoped only)
  │    decisions_parent = [parent_hook1, ..., fold_child]   ← child 结果作为一项参与父 reduce
  │    fold_final = reduce(&decisions_parent)
  │
  └─ fold_final 决定 tool 是否执行
```

这把 child 视为"一个额外的 hook"，保证父侧总能再 fold 一次 —— 与 Claude Code 文档"多 hook 并行 fold，`deny` 取胜"语义闭合。

> TODO(开放项)：subagent 自身的 `permission_mode = BypassPermissions` 时是否绕过 reducer？倾向 **不绕过 reducer 但 child hooks 全部短路返回 Allow** —— 这样 parent 仍能 deny。B6 落地。

---

## 8. `abort` 事件 wire schema + nextTurn 保留语义

```rust
// notification: server → client
// method: "session/aborted"
//
// 对齐 Pi: `{ type: "abort", clearedSteer: AgentMessage[], clearedFollowUp: AgentMessage[] }`
// 但 nextTurn 不出现在 abort 通知中（保留）。

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionAbortedNotification {
    pub session_id: SessionId,
    /// 当前 turn id（如果有）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// 被清空的 steer 队列内容（client 据此决定是否重发）
    pub cleared_steer: Vec<Item>,
    /// 被清空的 followUp 队列内容
    pub cleared_follow_up: Vec<Item>,
    /// **保留**的 next_turn 队列长度（语义提示：abort 后这些还在）
    pub next_turn_retained_count: u32,
}
```

### 8.1 与 Pi `abort()` 的对齐验收表（关键问题 #7）

| 维度 | Pi 行为（agent-harness.ts:936-963） | zhive 决策 | 对齐 |
|---|---|---|---|
| 清 steer | `this.steerQueue = []` | 同 | ✅ |
| 清 followUp | `this.followUpQueue = []` | 同 | ✅ |
| 清 nextTurn | **不清** | **不清** | ✅ |
| 取消 in-flight | `runAbortController?.abort()` | `cancel_token.cancel()` | ✅ |
| 等 idle | `await this.waitForIdle()` | 同（`runPromise.await`） | ✅ |
| 发送 abort 事件 | `emitOwn({ type: "abort", clearedSteer, clearedFollowUp })` | `session/aborted` notification | ✅（增加 `next_turn_retained_count` 提示） |
| 错误聚合 | `AggregateError([...])` | `thiserror` `#[from]` + `Vec<Cause>` | ✅ |
| 返回值 | `Promise<AbortResult>`（同 clearedSteer/clearedFollowUp） | `Result<AbortResult, AbortError>` | ✅ |

### 8.2 nextTurn 保留的对外文档措辞

> 当 client 调用 `session/cancel` 时：
> 1. 当前 turn 立即取消；in-flight tool_call 的 cancellation 取决于工具实现（zhive 不强制撤销已发的系统调用 / 网络请求）
> 2. `steer` 与 `followUp` 队列被清空，内容随 `session/aborted` notification 返回给 client
> 3. **`nextTurn` 队列保留**。client 可在 cancel 后立即 `enqueue_next_turn(...)` 追加消息，下一次 `session/prompt` 触发时这些消息会被 splice 到新 user message 之前
> 4. 所有 pending `permission/request` reverse-RPC 立即用 `Cancelled` outcome 响应（ACP 0.12 硬约束）

---

## 9. 关键问题逐条作答

| # | 问题 | 决策 | 理由 |
|---|---|---|---|
| 1 | PermissionDecision 四态序列化形式 | `#[serde(rename_all = "lowercase")]` ⇒ `"allow" / "deny" / "ask" / "defer"` | Claude Code SDK 字面要求；与 hooks docs example verbatim 对齐 |
| 2 | Reducer 签名 | `fn reduce(&[PermissionDecision]) -> PermissionDecision` —— 单纯 fn | reducer 无状态、无配置，trait 仅徒增间接；doctest 即可覆盖 |
| 3.A | 三队列分工 | Steer (turn 内)/FollowUp (turn 后)/NextTurn (跨 abort 保留) | Pi 一手代码 agent-harness.ts:183-187 + agent-loop.ts 时序闭合验证 |
| 3.B | 每队列独立 QueueMode | `Steer / FollowUp` 各自独立；`NextTurn` 无 mode（永远 All） | Pi: `steeringQueueMode + followUpQueueMode` 两个字段，`nextTurn` 直接 `splice(0)` 见 agent-harness.ts:534 |
| 4.A | Steer 触发时撤销 in-flight tool_call？ | **不撤** | Pi 模式 + 撤销已发 syscall 不可逆；steer 是"下一轮 LLM 视角"补丁 |
| 4.B | 已发的 reverse-request 回收？ | 用 ACP `Cancelled` outcome 显式响应所有 pending | ACP 0.12 schema 行 728-735 verbatim 硬约束 |
| 4.C | Steer 是否重置 turn 边界？ | **不重置** | turn 是 D-006 的逻辑单位；steer 是 inner-loop 内的注入；只有 abort/agent_end 关闭 turn |
| 5 | `Subagent.inherited_permissions` wire 字段？ | **不存在该字段** | 继承靠 `SubagentDefinition` 内 `Option` 字段缺省语义；与 Claude Code wire 对齐（无 explicit `inherited_permissions`） |
| 6 | 字段命名是否完全对齐 Claude Code？ | **完全对齐** | `hookSpecificOutput.permissionDecision / permissionDecisionReason / updatedInput / additionalContext / updatedToolOutput` 全部 `rename_all = "camelCase"` 直出；`hookEventName` 作 `#[serde(tag)]` |
| 7 | abort 语义是否完全对齐 Pi？ | **是**（含 nextTurn 保留），增 `next_turn_retained_count` 字段作语义提示 | 见 §8.1 对齐表全部 ✅ |

---

## 10. 未决项汇总

> TODO(开放项 A3-O1)：`ToolName` 类型由 A1 deliverable 决定（newtype `String` vs enum）。本 deliverable 临时占位 `pub struct ToolName(pub String)`。

> TODO(开放项 A3-O2)：`enqueue_next_turn` 在 `Idle` 入队后是否要主动通知 client。Pi 静默；zhive 是否发 `next_turn_queued` 事件？需要 B7 决定（影响 client 端 UI 提示策略）。

> TODO(开放项 A3-O3)：reverse-RPC `permission/request` 是否有 server 侧 timeout 默认。Pi 有 `setTimeout(...) → resolve(default)`；zhive 倾向加默认 30s `Deny` timeout（保护 server 资源），由 B6 落定。

> TODO(开放项 A3-O4)：subagent 自身 `permission_mode = BypassPermissions` 时 child reducer 是否完全短路。倾向"hooks 全返回 Allow，但 parent 仍能 deny"，由 B6 落地。

> TODO(开放项 A3-O5)：`HookSpecificOutput` 在 14 个事件下的完整 case 覆盖由 A4 完成。本 deliverable 仅给 `PreToolUse / PostToolUse` 两个示例 case。

> TODO(开放项 A3-O6)：`PermissionMode` 是否要加 `Plan` 之外的 `Inherit` 显式占位？目前 `Option<PermissionMode>` 的 `None` 即继承，无需 enum case；但若未来 wire 要"显式继承"可识别（比如 IDE 反查父 mode 时），可补 `PermissionMode::Inherit`。B6 决。

> TODO(开放项 A3-O7)：`session/aborted` 是 notification 还是 reverse-request？倾向 notification（client 无需回 ack），但 ACP 的 `session/cancel` 已是 client → server notification 单向；abort 是其响应，可能需要 result —— 待 B4（server transport）决定 wire 形态。

---

## 11. 建议 D-008 词条修订（送 `decision-diffs.md`）

```diff
- Schema 含 `StreamingBehavior: steer | followUp` 二元 mode（Pi 模型）
+ Schema 含 **三队列模型**（取代二元 mode，对齐 Pi agent-harness.ts:183-187）：
+   - `Steer`：turn 执行期间注入，对下一个 LLM 请求立即可见
+   - `FollowUp`：agent 无更多 action 时注入
+   - `NextTurn`：abort **不清空**，跨 turn 保留（恢复 / 重发关键）
+   每队列独立 `QueueMode { All | OneAtATime }`，NextTurn 无 mode（永远 All）。
+   Wire 上 `streamingBehavior?: "steer" | "followUp"` 二元仅覆盖前两个；
+   `NextTurn` 由独立 `session/next_turn` RPC method 驱动。
+ `abort()` 清 steer + followUp，**保留 nextTurn**；发
+   `session/aborted { clearedSteer, clearedFollowUp, nextTurnRetainedCount }` notification。
+ Pending `permission/request` 在 abort 时必须用 `Cancelled` outcome 响应
+   （ACP 0.12 硬约束，schema 行 728-735）。
```

---

## 12. 草图编译说明

本 deliverable 的 Rust 代码块**未提交到 crates/**。若要 sanity-check 类型可单文件 `rustc --edition 2024 --crate-type lib /tmp/a3.rs -L $(cargo path serde_json)`，依赖 `serde / serde_json / schemars`（已在 workspace）。生产代码落地点：

- `crates/zhive-proto/src/permission.rs`（B6 实现）
- `crates/zhive-proto/src/hook.rs`（A4 + B5 实现）
- `crates/zhive-proto/src/subagent.rs`（B8 实现）
- `crates/zhive-core/src/state/queues.rs`（B7 实现）
- `crates/zhive-core/src/state/abort.rs`（B7 实现）
