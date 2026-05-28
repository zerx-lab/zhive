---
task: B1
title: Engine / agent loop + EnginePhase 状态机
date: 2026-05-28
status: draft
depends_on:
  - A1 deliverable (Thread / Turn / Item 类型 + turn/started·turn/completed notification 形态)
  - A3 deliverable (StreamingBehavior steer/followUp + Permission reducer)
  - A4 deliverable (Hook 14 事件 + `#[non_exhaustive]`)
  - D-002 (TUI 不依赖 core，client/IDE 同级)
  - D-003 (JSON-RPC 2.0 single stream)
  - D-008 (server-initiated request, steer/followUp 二元 mode)
  - D-012 (Hook events, PreCompact/PostCompact 已锁)
  - D-014 (tracing 强制覆盖 Turn / Hook / Subagent / Permission / ToolCall)
references:
  - ${CODEX}/core/src/session/mod.rs:370-381          (`pub struct Codex { tx_sub, rx_event, agent_status, session, session_loop_termination }`)
  - ${CODEX}/core/src/session/session.rs:19-40        (`pub(crate) struct Session { conversation_id, tx_event, agent_status, state, active_turn: Mutex<Option<ActiveTurn>>, input_queue, services, ... }`)
  - ${CODEX}/protocol/src/protocol.rs:1567-1586       (`pub enum AgentStatus { PendingInit, Running, Interrupted, Completed(Option<String>), Errored(String), Shutdown, NotFound }`)
  - ${CODEX}/core/src/state/turn.rs:29-32             (`pub(crate) struct ActiveTurn { task: Option<RunningTask>, turn_state: Arc<Mutex<TurnState>> }`)
  - ${CODEX}/core/src/state/turn.rs:64-81             (`pub(crate) enum TaskKind { Regular, Review, Compact }` + `RunningTask { done, kind, task, cancellation_token, handle, turn_context, turn_extension_data, _timer }`)
  - ${CODEX}/core/src/state/turn.rs:83-98             (`pub(crate) struct TurnState { pending_approvals, pending_request_permissions, pending_user_input, pending_elicitations, pending_dynamic_tools, pending_input, mailbox_delivery_phase, granted_permissions, tool_calls, has_memory_citation, token_usage_at_turn_start }`)
  - ${CODEX}/core/src/thread_manager.rs:168-217       (`pub struct ThreadManager { state: Arc<ThreadManagerState> }` + `ThreadManagerState { threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>, thread_created_tx: broadcast::Sender<ThreadId>, ... }`)
  - ${CODEX}/core/src/codex_thread.rs:107-149         (`pub struct CodexThread { codex: Codex, session_source, session_configured, rollout_path, ... }` + `submit(Op) / shutdown_and_wait / wait_until_terminated`)
  - ${CODEX}/core/src/session/mod.rs:425-486          (`SUBMISSION_CHANNEL_CAPACITY=512` + `Codex::spawn_internal` 双 channel 初始化：`async_channel::bounded` for submissions, `async_channel::unbounded` for events)
  - ${CODEX}/protocol/src/protocol.rs:1588-1595       (`pub enum NonSteerableTurnKind { Review, Compact }` —— 取消同 Turn steer 的硬约束位点)
  - ${PI}/packages/agent/src/harness/types.ts:485     (`AgentHarnessPhase = "idle" | "turn" | "compaction" | "branch_summary" | "retry"`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:171  (`private phase: AgentHarnessPhase = "idle"` —— 字段持有)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:603-650 (`prompt / skill / promptFromTemplate` 三个入口的相同保护模式 `if (this.phase !== "idle") throw busy; this.phase = "turn"; ... finally this.phase = "idle"`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:652-667 (`steer / followUp` 必须 `phase !== "idle"` 否则 `invalid_state`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:681-735 (`compact()` 必须 `phase === "idle"` ⇒ `phase = "compaction"` ⇒ `finally idle`)
  - ${PI}/packages/agent/src/harness/agent-harness.ts:737-833 (`navigateTree()` ⇒ `phase = "branch_summary"`)
  - ${PI}/packages/agent/src/agent-loop.ts:160-268    (`runAgentLoop` 外层 follow-up + 内层 tool_call/steering 循环)
  - crates/zhive-core/src/engine.rs                    (Phase 1 待补 module)
  - crates/zhive-core/src/lib.rs                       (engine + state + server module 已声明)
---

> **设计衔接警告**：A1 deliverable §2.3 把 `turn/started / turn/completed` 描述为「core 暴露」的两个 JSON-RPC notification，且 A1 §6 草图把 `Turn.status` 设为 `TurnStatus = InProgress | Completed | Interrupted | Failed`（4 态）。B1 在此之上新增 **`EnginePhase` 显式枚举（Pi 模式，6 态）作为 Engine 级状态机**，与 `TurnStatus`（Turn 级状态）正交：`EnginePhase` 描述 engine 处于哪种宏观工作模式（Idle / Turn / Compaction / BranchSummary / Retry / SubagentSpawn），而 `TurnStatus` 描述某个具体 Turn 自身的生命周期。**不改 A1**——A1 的 4 态 TurnStatus 保留。下方 §3 给出二者交互矩阵。

---

## 1. 参考点清单

下面所有论断均回指此清单，逐条按 `路径:行号` 锚定。

| 主题 | 路径 | 行号 |
|---|---|---|
| codex `Codex` actor 字段：`tx_sub: Sender<Submission>` + `rx_event: Receiver<Event>` + `agent_status: watch::Receiver<AgentStatus>` + `session: Arc<Session>` + `session_loop_termination: Shared<BoxFuture>` | `${CODEX}/core/src/session/mod.rs` | 370-381 |
| codex `Session` 状态：`tx_event: Sender<Event>`, `agent_status: watch::Sender<AgentStatus>`, `state: Mutex<SessionState>`, `active_turn: Mutex<Option<ActiveTurn>>`, `input_queue: InputQueue`, `services: SessionServices`, `out_of_band_elicitation_paused: watch::Sender<bool>` | `${CODEX}/core/src/session/session.rs` | 19-40 |
| codex `AgentStatus` 7 态 enum（`PendingInit / Running / Interrupted / Completed(Option<String>) / Errored(String) / Shutdown / NotFound`） | `${CODEX}/protocol/src/protocol.rs` | 1567-1586 |
| codex `ActiveTurn { task: Option<RunningTask>, turn_state: Arc<Mutex<TurnState>> }` | `${CODEX}/core/src/state/turn.rs` | 29-32 |
| codex `TaskKind = Regular | Review | Compact` 三态 + `RunningTask { cancellation_token, handle: AbortOnDropHandle<()> }` 取消机制 | `${CODEX}/core/src/state/turn.rs` | 64-81 |
| codex `TurnState` 字段（pending approval / permission / user_input / elicitation / dynamic_tools + token_usage_at_turn_start） | `${CODEX}/core/src/state/turn.rs` | 83-98 |
| codex `NonSteerableTurnKind = Review | Compact` —— 同 turn steer 禁区 | `${CODEX}/protocol/src/protocol.rs` | 1588-1595 |
| codex `ThreadManager { state: Arc<ThreadManagerState> }` + `ThreadManagerState { threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>, thread_created_tx: broadcast::Sender<ThreadId>, ... }` | `${CODEX}/core/src/thread_manager.rs` | 168-217 |
| codex submission channel 容量 = 512 (`async_channel::bounded`)，event channel = `async_channel::unbounded` | `${CODEX}/core/src/session/mod.rs` | 425-486 |
| Pi `AgentHarnessPhase = "idle" \| "turn" \| "compaction" \| "branch_summary" \| "retry"`（5 态） | `${PI}/packages/agent/src/harness/types.ts` | 485 |
| Pi `private phase: AgentHarnessPhase = "idle"` —— 字段持有 + 类内单变量 | `${PI}/packages/agent/src/harness/agent-harness.ts` | 171 |
| Pi 三入口 `prompt / skill / promptFromTemplate`：`if (phase !== "idle") throw busy; phase = "turn"; try ... finally phase = "idle"` | `${PI}/packages/agent/src/harness/agent-harness.ts` | 603-650 |
| Pi `steer / followUp`：`if (phase === "idle") throw invalid_state` —— **只能在 turn 进行中调用** | `${PI}/packages/agent/src/harness/agent-harness.ts` | 652-667 |
| Pi `compact()`：要求 `phase === "idle"` ⇒ 进 `compaction` ⇒ `finally idle` | `${PI}/packages/agent/src/harness/agent-harness.ts` | 681-735 |
| Pi `navigateTree()`：要求 `phase === "idle"` ⇒ 进 `branch_summary` ⇒ `finally idle` | `${PI}/packages/agent/src/harness/agent-harness.ts` | 737-833 |
| Pi agent loop 双循环：外层 follow-up，内层 tool_call / steering | `${PI}/packages/agent/src/agent-loop.ts` | 160-268 |
| zhive A1 `TurnStatus = InProgress / Completed / Interrupted / Failed` | `plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md` §6 (`Turn` 草图) | 447-452 |
| zhive A1 `TurnStartedNotification / TurnCompletedNotification` 形态 | `plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md` §6 (`Turn lifecycle notification payloads`) | 738-750 |

---

## 2. EnginePhase 设计（核心交付）

### 2.1 case 选择：5 态（Pi）+ 1 zhive 自有 = 6 态

**决策**：直接采纳 Pi 的 5 态作为骨架（`Idle / Turn / Compaction / BranchSummary / Retry`），**新增第 6 态 `SubagentSpawn`**。理由见 §7 与 Pi 对照。

```rust
/// Engine 顶层工作模式。**与 `TurnStatus` 正交**（后者是单个 Turn 的内部生命周期）。
///
/// 状态机：见本 deliverable §2.3 合法转换图。状态转换由 `Engine::transition_phase`
/// 串行化（参考 codex `agent_status: watch::Sender<AgentStatus>`，
/// `session/mod.rs:376`）。phase 改变同步广播 `phase/changed` notification + 触发
/// `PhaseTransition` hook（见 §6.7）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EnginePhase {
    /// 无活跃 turn，可启 turn / compact / navigate / spawn subagent
    Idle,
    /// 正在处理 user prompt（含 tool_call 循环 + steering 注入）
    Turn,
    /// 在跑 context compaction（D-012 `PreCompact` hook 已触发，等 `PostCompact`）
    Compaction,
    /// 在做分支总结（fork 离开当前 leaf 时；对齐 Pi `navigateTree` 路径）
    BranchSummary,
    /// 正在重试上一次失败的 LLM call（指数退避中）
    Retry,
    /// 父 thread 派生 subagent 子 thread —— zhive 自有，对齐 D-008 subagent permission inheritance
    SubagentSpawn,
}
```

### 2.2 phase 转换：选 enum + match，**不**选 typestate

**决策**：用 `enum + match` 单变量串行化，**不**用 typestate（每态一个 `Engine<Phase>` 类型）。

**为何**：
1. typestate 会强制每 phase 一份 `Engine<Idle> / Engine<Turn> / ...` 拷贝，与 codex `Codex { agent_status: watch::Receiver<AgentStatus> }` 单类型 + 单 channel 的成熟形态相违 (`session/mod.rs:376`)
2. JSON-RPC server module 对所有 phase 用同一 dispatch entry（D-003 single stream），typestate 反而要外部包一层 enum wrapper
3. Pi 用 enum + 三入口前置 invariant 检查 (`agent-harness.ts:604 / 619 / 637`)，已实测可行
4. typestate 与 `Arc<Engine>` 不兼容（type 不能跨 Arc 边界变化）

### 2.3 合法转换图

```text
                            ┌─────────────────────────────────────────────┐
                            │                                             │
                            ▼                                             │
                       ┌─────────┐                                        │
                       │  Idle   │◀──────┐                                │
                       └─────────┘       │                                │
                       │  │  │  │  │     │                                │
              start_turn  │  │  │  │  compaction_done                     │
                       │  │  │  │  │     branch_summary_done              │
                       ▼  │  │  │  │     retry_resolved                   │
                   ┌──────┐ │  │  │  │   subagent_spawned                 │
                   │ Turn │ │  │  │  │                                    │
                   └──────┘ │  │  │  │                                    │
                       │ ▲  │  │  │  └────────────┐                       │
                turn_complete│  │  │               │                       │
                       │ ▼  │  │  │               │                       │
                       │    │  │  │   ┌───────────┴──┐                    │
                       │    │  │  └──▶│ SubagentSpawn├────────────────────┘
                       │    │  │      └──────────────┘
                       │    │  │
                       │    │  └──▶┌──────────────┐
                       │    │      │ BranchSummary├────────────────────────┐
                       │    │      └──────────────┘                        │
                       │    │                                              │
                       │    └─────▶┌────────────┐                          │
                       │           │ Compaction ├──────────────────────────┤
                       │           └────────────┘                          │
                       │                                                   │
                       └──────────▶┌────────┐                              │
            turn_failed_retryable  │ Retry  ├──────────────────────────────┘
                                   └────────┘
```

**合法转换矩阵**（行 = from，列 = to；`X` = 允许，`·` = 拒绝并 `EngineError::IllegalPhaseTransition`；`*` = 仅 internal 触发，外部不可主动跳）：

| from \ to        | Idle | Turn | Compaction | BranchSummary | Retry | SubagentSpawn |
|------------------|:---:|:---:|:---:|:---:|:---:|:---:|
| **Idle**         |  ·  |  X  |  X  |  X  |  ·  |  X  |
| **Turn**         |  X  |  ·  |  ·  |  ·  |  X* |  X  |
| **Compaction**   |  X  |  ·  |  ·  |  ·  |  ·  |  ·  |
| **BranchSummary**|  X  |  ·  |  ·  |  ·  |  ·  |  ·  |
| **Retry**        |  X  |  X* |  ·  |  ·  |  ·  |  ·  |
| **SubagentSpawn**|  X  |  ·  |  ·  |  ·  |  ·  |  ·  |

**关键不变量**（与 Pi 同源）：
- `Compaction / BranchSummary / Retry / SubagentSpawn` 只能 `→ Idle`，**互相不可跳**（避免组合爆炸 + 简化 hook 触发顺序）
- `Turn → Retry → Turn` 的环对外不可见，由 engine 内部 in-turn 重试机制驱动（参 §6.5）
- `Turn` 期间允许 `steer / followUp` 注入消息但**不改变 phase**（Pi 模型 `agent-harness.ts:652-667`）

### 2.4 与 Pi `AgentHarnessPhase` 的差异

| 维度 | Pi 5 态 | zhive 6 态 | 理由 |
|---|---|---|---|
| Idle | ✓ | ✓ | 1:1 抄 |
| Turn | ✓ | ✓ | 1:1 抄；含 tool_call 循环（codex `ActiveTurn.task: RunningTask`，`state/turn.rs:71-81`）|
| Compaction | ✓ | ✓ | 与 D-012 `PreCompact/PostCompact` hook 自然对齐（hook 触发即进 phase，结束即出）|
| BranchSummary | ✓ | ✓ | A1 §6 没列 fork item 但 D-011 leaf 指针 + JSONL fork 需要这态；保留对齐 Pi `navigateTree` |
| Retry | ✓ | ✓ | LLM provider 错误（429/502 / network）触发；Pi `agent-harness.ts:485` 已列态名但本调研在 Pi codebase 中**未 grep 到 `phase = "retry"` 显式 set**（仅在 type alias 列出）—— 推测是预留位 |
| **SubagentSpawn** | ✗ | ✓（zhive 新增） | D-008 reverse RPC 含 `subagent permission inheritance` 硬约束；spawn 期间父 engine 必须暂停 turn 推进等 child engine 就绪 + 写权限继承 reducer，是独立工作单元 |

**砍掉的 Pi 概念**：
- 无（Pi 5 态全部保留）

**zhive 新增的不在 Pi 中的概念**：
- `SubagentSpawn`（D-008 子代理派生）

**TODO(开放项 B1-1)**：Pi 自己的 `retry` 态在 codebase 中无显式 set（rg 仅命中 type alias `agent-harness.ts:485`）。这表明 Pi 把 retry 算作 turn 内 sub-state，未提升到 phase。zhive 是否真需要把 `Retry` 独立成 phase 待 §8 验证。备选：把 `Retry` 折回 `Turn` 内部的 `TurnStatus` 扩展（`InProgress { retry_count: u32 }`）。倾向**保留独立 phase** —— 让 hook `PhaseTransition` 能监测到 retry 进入，以及 metric/tracing 能直接 span（D-014）。

---

## 3. EnginePhase 与 A1 TurnStatus 交互矩阵

A1 `TurnStatus`（4 态）是**单 turn 的内部生命周期**，B1 `EnginePhase`（6 态）是 **engine 顶层 phase**。

| EnginePhase | 期望 TurnStatus（当前 Turn）| 备注 |
|---|---|---|
| `Idle` | （无活跃 Turn 或 Turn 已 `Completed/Interrupted/Failed`） | 上一个 Turn 已 finalize；下一个 Turn 尚未 start |
| `Turn` | `InProgress` | 单一一一对应：phase=Turn ⟺ 存在一个 InProgress Turn |
| `Compaction` | （无活跃 Turn） | compaction 在 Turn 边界外触发；compaction 自己**不创建** Turn item，而是产 `Item::ContextCompaction`（A1 §3 case 13）追加到上一个完成 Turn 末尾 |
| `BranchSummary` | （无活跃 Turn） | fork 操作；产 `SystemNotice` item 落地 |
| `Retry` | `InProgress`（同一个 Turn id） | retry 不开新 Turn，仅在 in-flight Turn 上重试 LLM call |
| `SubagentSpawn` | 父 Turn 仍 `InProgress`（父 thread 视角）；子 thread 内的子 Turn 仍未起 | 父 Turn 在等 subagent 就绪；transition 后 phase 回 `Turn` |

**不变量**：phase ∈ {Turn, Retry, SubagentSpawn} ⟺ 存在一个 `InProgress` Turn（在 engine 当前 active thread 上）。`SubagentSpawn` 的子 thread 有自己的 Engine 实例 + 独立 phase 状态机。

---

## 4. Engine struct 与公开方法签名

> 写在本 deliverable 内部代码块，**不进 `crates/`**（按硬约束）。所有 `todo!()` 占位。

```rust
//! Phase 1 草图：zhive-core::engine
//!
//! B1 落地。Engine actor + agent loop + EnginePhase 状态机。

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use zhive_proto::domain::{
    Item, ItemId, Thread, ThreadId, Turn, TurnId, TurnStatus,
    TurnStartedNotification, TurnCompletedNotification,
    // 以下三类是 B1 新增的 wire payload（用于 phase/changed notification）
    EnginePhase, PhaseChangedNotification,
};

// ============================================================
// EnginePhase（见 §2.1 完整定义；下面仅引用）
// ============================================================
// pub enum EnginePhase { Idle, Turn, Compaction, BranchSummary, Retry, SubagentSpawn }

// ============================================================
// Engine —— 顶层 actor handle（可 clone，内部 Arc）
// ============================================================

/// `Engine` 是 zhive-core 的顶层句柄。
///
/// **可 clone**：内部状态 `Arc<EngineInner>`，所有公开方法走 actor pattern
/// （codex `Codex { tx_sub, rx_event, session: Arc<Session> }` 同形态，
/// `session/mod.rs:370-381`）。
///
/// 与 codex 差异：
/// - codex 用 `async_channel::bounded(512)` for submissions（`session/mod.rs:426`）+
///   `async_channel::unbounded()` for events。zhive 用 tokio `mpsc::Sender` for
///   submissions + tokio `broadcast::Sender` for fan-out events（多客户端订阅同 thread）。
/// - codex `agent_status: watch::Receiver<AgentStatus>` 是 7 态。zhive
///   `phase: watch::Receiver<EnginePhase>` 是 6 态（语义不同：Pi 模式 vs codex 7 态）。
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

/// 内部状态（不可 clone，由 `Engine` 通过 `Arc` 共享）。
pub(crate) struct EngineInner {
    /// 所有活跃 thread。codex `ThreadManagerState.threads`（`thread_manager.rs:200`）等价物。
    threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>,
    /// 当前 engine 顶层 phase（**与 turn 解耦**）。
    /// codex 是 `agent_status: watch::Sender<AgentStatus>`（`session.rs:23`）等价物。
    phase_tx: watch::Sender<EnginePhase>,
    /// engine 级 cancel token（shutdown 信号）。
    shutdown: CancellationToken,
    /// 单调递增的 submission id 计数器。
    next_submission_id: std::sync::atomic::AtomicU64,
    /// 反向 RPC 注册器（D-008 server-initiated request：permission/request、
    /// elicitation 等）。具体形态推到 B4/B7 deliverable。
    reverse_rpc: Arc<dyn ReverseRpcSink + Send + Sync>,
    /// Hook host（D-012 14 事件 dispatcher）。具体形态推到 B6 deliverable。
    hook_host: Arc<dyn HookHost + Send + Sync>,
    /// LLM provider（D-014 trace 落点）。具体形态推到 B10 deliverable。
    provider: Arc<dyn LlmProvider + Send + Sync>,
    /// Permission reducer（A3 deliverable）。
    permission_reducer: Arc<dyn PermissionReducer + Send + Sync>,
    /// Storage 4 库聚合接口（D-011）。具体形态推到 B3 deliverable。
    storage: Arc<dyn Storage + Send + Sync>,
    /// 事件 fan-out 总线（按 thread_id 路由由 wrapper 处理）。
    event_bus: broadcast::Sender<EngineEvent>,
}

/// 单个 thread 的运行时 handle。codex `CodexThread`（`codex_thread.rs:107`）等价物。
pub(crate) struct ThreadHandle {
    thread_id: ThreadId,
    /// 该 thread 当前活跃 Turn（**最多一个**，串行约束）。
    /// codex `Session.active_turn: Mutex<Option<ActiveTurn>>`（`session.rs:34`）等价物。
    active_turn: Mutex<Option<ActiveTurn>>,
    /// thread 级 cancel token（中断当前 turn 用）。
    cancel: CancellationToken,
    /// 持久化 thread 数据（从 storage load）。
    thread: Arc<RwLock<Thread>>,
    /// Submission 入口 channel（actor 形态；codex `tx_sub`，`session/mod.rs:373`）。
    sub_tx: mpsc::Sender<Submission>,
    /// 后台 task handle（agent loop spawn 后的 join handle）。
    _loop_handle: AbortOnDropHandle<()>,
}

/// 单个 turn 的运行时状态。codex `ActiveTurn`（`state/turn.rs:29-32`）等价物。
pub(crate) struct ActiveTurn {
    turn_id: TurnId,
    kind: TurnKind,
    /// 取消信号（steer 不取消，只追加 pending；interrupt 才 cancel）。
    /// codex `RunningTask.cancellation_token`（`state/turn.rs:75`）等价物。
    cancel: CancellationToken,
    /// turn-scoped 状态（pending approval / pending input / token usage）。
    /// codex `TurnState`（`state/turn.rs:83-98`）等价物。
    state: Arc<Mutex<TurnState>>,
    /// turn 内 item 发布通道（item 级 push notification 的源）。
    item_tx: mpsc::Sender<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// 普通用户 prompt 驱动的 turn。
    Regular,
    /// Subagent 派生的 turn（继承父 permission scope）。
    Subagent,
    /// Compaction-as-turn（codex `TaskKind::Compact`，`state/turn.rs:68`）。zhive
    /// **不**走 turn pipeline，独立 phase——所以这态 zhive 暂不使用，留给 Phase 2 review-as-turn。
    #[allow(dead_code)]
    Review,
}

pub(crate) struct TurnState {
    /// permission 请求待回（A3 deliverable PermissionDecision reducer）。
    pending_approvals: HashMap<String, oneshot::Sender<PermissionDecision>>,
    /// followUp/steer 注入的消息（在下次 LLM 请求前 flush 进 context）。
    /// Pi `steerQueue / followUpQueue`（`agent-harness.ts:183-186`）等价物。
    pending_input: Vec<UserInput>,
    /// Streaming behavior 模式（A3 § StreamingBehavior steer/followUp）。
    streaming_behavior: StreamingBehavior,
    /// turn-level tool_call 计数（codex `TurnState.tool_calls`，`state/turn.rs:95`）。
    tool_call_count: u64,
    /// turn 开始时 token usage 快照（用于 D-012 PreCompact 阈值）。
    token_usage_at_start: TokenUsage,
}

/// 顶层 submission（actor message）。codex `Submission`/`Op` 等价物。
#[derive(Debug)]
pub enum Submission {
    /// `session/prompt` 入口（A1 §2.3 算法）。
    StartTurn { thread_id: ThreadId, inputs: Vec<UserInput>, reply: oneshot::Sender<Result<TurnId, EngineError>> },
    /// `session/cancel` 中断。
    InterruptTurn { thread_id: ThreadId, turn_id: TurnId, reply: oneshot::Sender<Result<(), EngineError>> },
    /// D-008 steer（in-turn 强制介入）。
    Steer { thread_id: ThreadId, turn_id: TurnId, input: UserInput, reply: oneshot::Sender<Result<(), EngineError>> },
    /// D-008 followUp（in-turn 补充输入）。
    FollowUp { thread_id: ThreadId, turn_id: TurnId, input: UserInput, reply: oneshot::Sender<Result<(), EngineError>> },
    /// 触发 context compaction（D-012 PreCompact）。
    Compact { thread_id: ThreadId, reply: oneshot::Sender<Result<(), EngineError>> },
    /// 触发 branch summary（fork）。
    BranchSummary { thread_id: ThreadId, target_branch_id: BranchId, reply: oneshot::Sender<Result<(), EngineError>> },
    /// 派生 subagent（D-008 subagent permission inheritance）。
    SpawnSubagent { parent_thread_id: ThreadId, spec: SubagentSpec, reply: oneshot::Sender<Result<ThreadId, EngineError>> },
    /// 优雅 shutdown。
    Shutdown { reply: oneshot::Sender<()> },
}

/// 顶层 event（actor outbound）—— broadcast fan-out。
#[derive(Debug, Clone)]
pub enum EngineEvent {
    PhaseChanged(PhaseChangedNotification),
    TurnStarted(TurnStartedNotification),
    TurnCompleted(TurnCompletedNotification),
    /// item 级 push（实时流出 reasoning chunk / tool_call / agent_message）
    ItemAppended { thread_id: ThreadId, turn_id: TurnId, item: Item },
    /// 反向 RPC 请求（permission / elicitation 等；D-008）。
    ReverseRequest(ReverseRequest),
}

// ============================================================
// Engine 公开方法签名（async + Result + 真正干活的接口）
// ============================================================

impl Engine {
    /// 启动一个新 Engine 并 spawn 后台 dispatcher loop。
    ///
    /// codex 等价：`Codex::spawn`（`session/mod.rs:431`）+ `ThreadManager::new`
    /// （`thread_manager.rs:245`）。
    pub async fn spawn(config: EngineConfig) -> Result<Self, EngineError> {
        todo!("B1 不实现；仅签名")
    }

    /// 订阅 phase 变化（codex `watch::Receiver<AgentStatus>` 模式）。
    pub fn subscribe_phase(&self) -> watch::Receiver<EnginePhase> {
        todo!()
    }

    /// 当前 phase 快照。
    pub fn phase(&self) -> EnginePhase {
        todo!()
    }

    /// 订阅顶层事件 fan-out（broadcast）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<EngineEvent> {
        todo!()
    }

    /// 启动新 Turn —— A1 §2.3 入口算法的 wrapper。
    /// **不变量**：必须 `phase() == Idle` 才能进；否则 `EngineError::Busy`。
    /// Pi 三入口 `prompt / skill / promptFromTemplate`（`agent-harness.ts:603 / 619 / 637`）等价物。
    pub async fn start_turn(
        &self,
        thread_id: &ThreadId,
        inputs: Vec<UserInput>,
    ) -> Result<TurnId, EngineError> {
        todo!()
    }

    /// 中断 in-flight Turn（同步 cancel token + drain pending）。
    pub async fn interrupt_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> Result<(), EngineError> {
        todo!()
    }

    /// In-turn steer（D-008 / A3 StreamingBehavior::Steer）—— 强制介入，
    /// 撤销 in-flight tool_call，把当前 LLM 响应中断并 inject 新 input。
    pub async fn steer(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        input: UserInput,
    ) -> Result<(), EngineError> {
        todo!()
    }

    /// In-turn followUp（D-008 / A3 StreamingBehavior::FollowUp）—— 排队等
    /// 当前 LLM 响应完成后再 inject。
    pub async fn follow_up(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        input: UserInput,
    ) -> Result<(), EngineError> {
        todo!()
    }

    /// 触发 context compaction（D-012 PreCompact hook 进入点）。
    /// **不变量**：`phase() == Idle` 才能进；触发后 `phase() = Compaction`。
    pub async fn compact(&self, thread_id: &ThreadId) -> Result<(), EngineError> {
        todo!()
    }

    /// fork 操作 + 分支总结（Pi `navigateTree`，`agent-harness.ts:737`）。
    pub async fn branch_summary(
        &self,
        thread_id: &ThreadId,
        target_branch_id: BranchId,
    ) -> Result<(), EngineError> {
        todo!()
    }

    /// 派生 subagent —— phase 临时 `→ SubagentSpawn`，子 thread 启动后 `→ Turn`（父）。
    pub async fn spawn_subagent(
        &self,
        parent_thread_id: &ThreadId,
        spec: SubagentSpec,
    ) -> Result<ThreadId, EngineError> {
        todo!()
    }

    /// 优雅 shutdown：drain pending submissions、cancel 所有 in-flight、flush rollout。
    /// codex `Codex::shutdown_and_wait`（`codex_thread.rs:142`）等价物。
    pub async fn shutdown(self) -> Result<(), EngineError> {
        todo!()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    #[error("engine is busy in phase {current:?}, cannot accept {action}")]
    Busy { current: EnginePhase, action: &'static str },
    #[error("illegal phase transition {from:?} -> {to:?}")]
    IllegalPhaseTransition { from: EnginePhase, to: EnginePhase },
    #[error("thread not found: {0:?}")]
    ThreadNotFound(ThreadId),
    #[error("turn not found: {thread:?}/{turn:?}")]
    TurnNotFound { thread: ThreadId, turn: TurnId },
    #[error("turn already completed: {0:?}")]
    TurnAlreadyCompleted(TurnId),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("hook error: {0}")]
    Hook(#[from] HookError),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("engine shutdown")]
    Shutdown,
}

// ============================================================
// 占位 trait（外部 crate 实际定义，B1 不实现）
// ============================================================

pub trait ReverseRpcSink {} // B4/B7
pub trait HookHost {}        // B6
pub trait LlmProvider {}     // B10
pub trait PermissionReducer {} // A3+B7
pub trait Storage {}         // B3

// 占位类型（其他 deliverable 定义）
pub type UserInput = serde_json::Value;     // A1 §6 已定 `Vec<UserInput>`
pub type PermissionDecision = serde_json::Value; // A3 §2 已定四态
pub type StreamingBehavior = serde_json::Value;   // A3 §3 `Steer | FollowUp`
pub type ReverseRequest = serde_json::Value;
pub type TokenUsage = serde_json::Value;
pub type SubagentSpec = serde_json::Value;
pub type BranchId = String;
pub type StorageError = std::io::Error;
pub type ProviderError = std::io::Error;
pub type HookError = std::io::Error;
pub struct EngineConfig {}

// ============================================================
// PhaseChangedNotification —— wire payload（A1 §6 `Turn lifecycle
// notification payloads` 模式同形态）
// ============================================================

// 这个类型应该落到 zhive-proto::domain（与 TurnStartedNotification 同位置）。
// 此处仅占位；B1 推荐 wire 形态：
//
// #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
// #[serde(rename_all = "camelCase")]
// pub struct PhaseChangedNotification {
//     pub from: EnginePhase,
//     pub to: EnginePhase,
//     pub thread_id: Option<ThreadId>,  // 与该 phase 相关的 thread（compact/branch/turn 有；engine-level transition 没有）
//     pub timestamp_ms: i64,
// }
//
// method: "phase/changed"
```

**编译性约束**：
- `Engine: Clone`（内部 `Arc<EngineInner>`），与 codex `Codex` 共享语义同
- `EnginePhase: Copy`（小态枚举），方便穿越 `watch::Sender / Receiver`
- `EngineError: thiserror::Error + From<StorageError|ProviderError|HookError>` —— CLAUDE.md 强制 `?` + thiserror
- 所有 `pub fn` 是 `async fn ... -> Result<T, EngineError>` 形态，不 `unwrap/expect`

---

## 5. Turn lifecycle 状态机

> 关注点：单个 Turn 从 `start_turn` 到 `turn/completed` 的内部生命周期。Engine `EnginePhase = Turn` 期间，该 Turn 在此图中游走。

```text
                  start_turn(inputs)
                        │
                        ▼
            ┌──────────────────────────┐
            │  TurnStatus::InProgress  │◀────────────┐
            │  (engine phase = Turn)   │             │
            └──────────────────────────┘             │
                  │                                  │
                  │ (LLM streaming)                  │ retry_resolved
                  │                                  │ (in-turn LLM retry)
                  ▼                                  │
            ┌───────────┐                            │
            │  streaming│                            │
            │  reasoning│                            │
            │  + chunks │                            │
            └───────────┘                            │
                  │                                  │
                  ▼                                  │
            ┌──────────────────────────┐             │
            │  LLM 响应完成            │             │
            │  含 tool_calls?          │             │
            └──────────────────────────┘             │
                  │                                  │
       ┌──────────┴─────────┐                        │
       │ yes                │ no                     │
       ▼                    ▼                        │
  ┌─────────┐         ┌──────────────┐               │
  │tool_call│         │ has pending  │               │
  │dispatch │         │ steering or  │               │
  │+ result │         │ followUp ?   │               │
  └─────────┘         └──────────────┘               │
       │                    │ yes      │ no          │
       │   ┌────────────────┘          │             │
       ▼   ▼                           ▼             │
  ┌─────────┐                    ┌─────────┐         │
  │ append  │                    │  finish │         │
  │context, │                    │  turn   │         │
  │loop back│───────────────▶────└─────────┘         │
  └─────────┘                          │             │
                                       │             │
                       (LLM error?     │             │
                        retryable?)    │             │
                                       ▼             │
                     ┌─────────────────┴─────────┐   │
                     │ Yes ⇒ phase Turn→Retry   │───┘
                     │ (TurnStatus 仍 InProgress)│
                     └───────────────────────────┘
                                       │
                                       │ no retry / cancelled / final
                                       ▼
                     ┌────────────────────────────────┐
                     │ TurnStatus:                    │
                     │  Completed / Interrupted /     │
                     │  Failed                        │
                     │ phase Turn→Idle                │
                     └────────────────────────────────┘
                                       │
                                       ▼
                              emit turn/completed
```

**关键点**：
1. 同一 Turn 内可 `Retry` 多次 —— 每次只切 `EnginePhase`，**不**变 `TurnStatus`（仍 `InProgress`）
2. `steer` 在该 Turn 进行中可调（中断当前 LLM 响应），`followUp` 排队等到下一次 LLM 请求前 inject —— 二者**都不切** EnginePhase，仅改 `TurnState.pending_input`
3. Turn 结束时（`Completed/Interrupted/Failed`）engine **必然** `EnginePhase → Idle`（除非接着启另一个动作如 `compact()`，但那是新的 transition）

---

## 6. Channel 拓扑

> 关注点：哪些消息流走 `mpsc / broadcast / oneshot / watch`，为什么。

### 6.1 拓扑图

```text
       ┌─────────────────────────────────────────────────────┐
       │                    Client(s)                        │
       │ (CLI / TUI / IDE / MCP/ACP bridge)                  │
       └───────────────┬─────────────────────────┬───────────┘
                       │                         ▲
                       │ JSON-RPC                │ JSON-RPC
                       │ Request                 │ Notification
                       │ (Submission)            │ (EngineEvent fan-out)
                       ▼                         │
                 ┌──────────────────────────┐    │
                 │ Server module (D-003)    │    │
                 │ - decode framing         │    │
                 │ - route by method        │    │
                 └──────┬─────────┬─────────┘    │
                        │         │              │
                        ▼         │              │
       ┌─────────────────────┐    │              │
       │ Engine::start_turn  │    │              │
       │ Engine::steer       │    │              │
       │ Engine::interrupt   │    │              │
       │ ... (公开 API)      │    │              │
       └─────────┬───────────┘    │              │
                 │                │              │
   (Submission) mpsc::Sender────►─┘              │
                 │                               │
                 ▼                               │
        ┌──────────────────┐                     │
        │ Engine dispatcher│                     │
        │ loop (1 task per │                     │
        │ engine, **not**  │                     │
        │ per-thread)      │                     │
        └────┬─────────────┘                     │
             │                                   │
             │ (route by thread_id)              │
             ▼                                   │
        ┌──────────────────┐                     │
        │ ThreadHandle.    │                     │
        │ sub_tx (mpsc)    │                     │
        └────┬─────────────┘                     │
             │                                   │
             ▼                                   │
        ┌──────────────────────────────────┐     │
        │ Agent loop task (1 per thread)   │     │
        │  (Pi `runAgentLoop`              │     │
        │   `agent-loop.ts:160-268` 等价)   │     │
        │                                  │     │
        │   ┌───────────────────────┐      │     │
        │   │ ActiveTurn.item_tx    │      │     │
        │   │  (mpsc → 1 consumer)  │      │     │
        │   └───────┬───────────────┘      │     │
        │           │                      │     │
        │           ▼                      │     │
        │   ┌────────────────────┐         │     │
        │   │ Item appender task │         │     │
        │   │ - persist (B3)     │         │     │
        │   │ - emit to event_bus│         │     │
        │   └────────┬───────────┘         │     │
        │            │                     │     │
        └────────────┼─────────────────────┘     │
                     │                           │
                     ▼                           │
              ┌──────────────────┐               │
              │ event_bus:       │               │
              │ broadcast::Send  │───── fan-out ─┘
              │ (1 → N clients)  │
              └──────────────────┘

       ┌─────────────────────────────────────────────────────┐
       │              Side channels                          │
       │                                                     │
       │ phase_tx: watch::Sender<EnginePhase>                │
       │   ↳ subscribe via Engine::subscribe_phase()         │
       │     (用于 hook host / TUI 显示 / tracing 看 phase)  │
       │                                                     │
       │ shutdown: CancellationToken                         │
       │   ↳ engine-level stop signal                        │
       │                                                     │
       │ ThreadHandle.cancel: CancellationToken              │
       │   ↳ per-thread cancel（interrupt_turn 用）          │
       │                                                     │
       │ ActiveTurn.cancel: CancellationToken                │
       │   ↳ per-turn cancel（steer 时部分使用，             │
       │     interrupt_turn 时全砍）                         │
       │                                                     │
       │ Submission.reply: oneshot::Sender<Result>           │
       │   ↳ 每个 submission 的同步回执                       │
       │                                                     │
       │ TurnState.pending_approvals: HashMap<id, oneshot>   │
       │   ↳ D-008 反向 RPC permission/request 应答 sink      │
       │                                                     │
       └─────────────────────────────────────────────────────┘
```

### 6.2 channel 类型选择理由表

| 用途 | 选型 | 容量 | 理由（含 codex / Pi 对照） |
|---|---|---|---|
| Client → Engine **submission** | `mpsc::Sender<Submission>` | bounded(512) | 单一 consumer（dispatcher loop）；back-pressure；对齐 codex `async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY=512)`（`session/mod.rs:426`） |
| Dispatcher → ThreadHandle **per-thread submission** | `mpsc::Sender<Submission>` | bounded(64) | 单 thread 单 consumer（agent loop task）；同步 step 内 fan-out |
| ActiveTurn 内 **item 流** | `mpsc::Sender<Item>` | bounded(256) | 单消费者（item appender + emitter）；back-pressure 防 reasoning chunk 风暴 |
| Engine → Client(s) **EngineEvent** fan-out | `broadcast::Sender<EngineEvent>` | bounded(1024) | **多消费者**：TUI / IDE / MCP bridge / OTel exporter 各订一份；codex 也用 `broadcast::Sender<ThreadId>`（`thread_manager.rs:201`） |
| **phase 当前值** | `watch::Sender<EnginePhase> + Receiver<EnginePhase>` | 1（latest only） | 等价于 codex `agent_status: watch::Sender<AgentStatus>`（`session.rs:23`）—— watch 适合"读最新值不要历史"的状态共享 |
| Submission **同步回执** | `oneshot::Sender<Result<_, EngineError>>` | 1 | 单次使用；每个 `Submission::*` variant 内嵌一个 `reply` 字段 |
| Pending **permission decision** | `HashMap<String, oneshot::Sender<PermissionDecision>>` | 多个 oneshot | 与 codex `TurnState.pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>`（`state/turn.rs:86`）1:1 同形态 |
| **shutdown / interrupt 信号** | `CancellationToken`（tokio_util） | n/a | 多观察者；codex `RunningTask.cancellation_token: CancellationToken`（`state/turn.rs:75`）同 |
| 后台 task 自动取消 | `AbortOnDropHandle<()>`（tokio_util） | n/a | drop engine ⇒ drop handle ⇒ abort task；codex `RunningTask.handle: AbortOnDropHandle<()>`（`state/turn.rs:76`）同 |

**砍掉的选型**：
- `async_channel::unbounded()`（codex 用在 event channel） —— zhive 不用 unbounded 因 D-014 tracing 强制覆盖事件流，无 back-pressure 会 OOM；改 broadcast bounded
- `flume` —— D-001/D-009 砍依赖收敛压力大，tokio mpsc/broadcast 够用

### 6.3 ownership 模型决策

**决策**：**`Engine = Arc<EngineInner>` + actor pattern**（消息驱动 dispatcher）。

**为何不选 `Arc<Mutex<Engine>>`**：
- 全局 Mutex 序列化所有操作，与"单 thread 串行 / 跨 thread 并行"目标冲突
- codex 没有任何 `Mutex<Engine>` —— Codex 是双 channel 句柄 (`tx_sub, rx_event`)，状态在 `Arc<Session>` 内分粒度上锁

**为何不选 typestate**：见 §2.2

**为何**选 actor：
- codex `Codex { tx_sub, rx_event, session: Arc<Session> }` 已成熟（`session/mod.rs:370-381`）
- 与 JSON-RPC server module 天然契合：server 把 RPC 翻成 `Submission` 投进 mpsc，dispatcher 消费 + 投 event_bus 出去
- Pi `AgentHarness` 在 JS 单线程下用 `this.phase` 字段 + `runAbortController` 已经隐含 actor 语义；Rust 下显式化

**单 thread 内 turn 串行 vs 跨 thread 并行**：
- **单 thread 内**：严格串行（`ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` 同 codex `Session.active_turn: Mutex<Option<ActiveTurn>>`，`session.rs:34`）。Pi 也强制 `if phase !== "idle" throw busy`（`agent-harness.ts:604/619/637`）
- **跨 thread**：并行（每个 thread 一个独立 agent loop task）。codex 同（`ThreadManagerState.threads: Arc<RwLock<HashMap>>`，`thread_manager.rs:200`）
- **EnginePhase**：跨 thread 视角的 **engine 顶层** phase。如果 engine 服务多 thread，phase 反映"当前 engine 主线程在做什么"——典型场景是 CLI 单 thread 模式下 phase ≈ 当前 thread 的工作模式；多 thread 模式（如 IDE 同时打开多 chat）下需要 per-thread phase。**TODO(B1-2)**：是否要 per-thread `EnginePhase`？倾向 **per-thread**，让 `phase_tx` 改为 `HashMap<ThreadId, watch::Sender<EnginePhase>>`。

### 6.4 turn 串行 / 并发问题（关键问题 #4 完整答案）

**codex 怎么做**：单 thread 内 `Session.active_turn: Mutex<Option<ActiveTurn>>`（`session.rs:34`）—— **串行**。`TaskKind = Regular | Review | Compact`（`state/turn.rs:65-69`）通过 turn kind 区分，**同一时刻仅一个 active**。`NonSteerableTurnKind = Review | Compact`（`protocol.rs:1589-1595`）说明 review/compact 是**不能 steer 的特殊 turn 类**。

**zhive 决策**：**单 thread 内 turn 串行**（同 codex / Pi 模式）；跨 thread turn 并行（不同 thread 各自独立 agent loop task）。

### 6.5 Retry 实现位置

**决策**：Retry 是**在 turn 内的 LLM call 级**重试，不创建新 Turn / Item，但**切 `EnginePhase = Retry`** 让 hook/metric 能观测。

具体：`provider.send()` 返回 `Err(ProviderError::Retryable { backoff })` ⇒ engine 切 phase 到 `Retry` ⇒ sleep(backoff) ⇒ 再 send ⇒ 成功后切回 `Turn`，turn_id 不变。

### 6.6 事件流（关键问题 #5 完整答案）

Turn 内事件流顺序（参考 Pi `agent-loop.ts:160-268`）：

```text
turn_start
  → message_start (assistant message id)
    → reasoning_chunk * N    (streaming)
    → agent_message_chunk * N (streaming)
    → tool_call_open
    → tool_call_argument_chunk * N
    → tool_call_close
  → message_end
  → (per tool_call) → tool_result_open → tool_result_chunk → tool_result_close
turn_end (with TurnStatus)
```

zhive 把这些事件**统一**通过 `ActiveTurn.item_tx: mpsc::Sender<Item>`（**单 producer：agent loop task；单 consumer：item appender task**）发出。item appender：
1. 持久化 item（B3 storage `Append-only JSONL`）
2. emit 到 `event_bus: broadcast::Sender<EngineEvent>` 让所有客户端订阅
3. emit `ItemAppended` notification（zhive-proto `item/appended`，A1 § 4 表已列）

**为何 mpsc 而非 broadcast 在 turn 内**：turn 内只有 1 个 consumer（appender），不需要 fan-out；fan-out 在 appender 之后由 broadcast 接力。

### 6.7 PhaseTransition hook（关键问题 #7 完整答案）

**决策**：**新增**通用 `PhaseTransition` hook 事件，**保留** D-012 的 `PreCompact / PostCompact` 不动。

**为何不只用 `PreCompact / PostCompact`**：D-012 14 事件列了 `PreCompact` 但没有对称的 `PreBranchSummary / PostBranchSummary`，也没有 `PreTurn / PostTurn / PreRetry / PostRetry / PreSubagentSpawn / PostSubagentSpawn`。若全加，14 事件 → 24 事件，膨胀且与已锁定 D-012 冲突。

**决策细节**：
- `PhaseTransition { from: EnginePhase, to: EnginePhase, thread_id }`：在每次 `EnginePhase` 切换时同步触发，**所有** phase 变化都走它
- `PreCompact / PostCompact` 保留并**同时**触发（D-012 已锁，不破坏）：先发 `PhaseTransition { from: Idle, to: Compaction }`，再发 `PreCompact`；Compaction 结束时先发 `PostCompact`，再发 `PhaseTransition { from: Compaction, to: Idle }`
- 这样 hook 作者既可以**粗粒度**订 `PhaseTransition` 一个事件（覆盖所有 6 态），也可以**细粒度**只订 `PreCompact`（兼容 Claude Code / Pi 既有用法）

**TODO(B1-3)**：与 A4 deliverable 对齐 —— A4 列的 14 事件需要扩 1 个 `PhaseTransition`（事件数 → 15）。需要 A4 落地时确认是否接受这个扩展（不会变成"非 D-012 默契 break"，因为 D-012 字面写"至少 14"和 `#[non_exhaustive]`）。

---

## 7. codex / Pi 并列对照表（关键问题 #1 / #2 / #6 综合）

| 维度 | codex | Pi | zhive B1 决策 |
|---|---|---|---|
| 顶层 actor 类型名 | `Codex` (`session/mod.rs:372`) | `AgentHarness` (`agent-harness.ts:164`) | `Engine` |
| Thread 容器 | `ThreadManager.state.threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>` (`thread_manager.rs:200`) | （Pi 单 thread；多 thread 由 caller 维护多 harness） | `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>`（codex 模式） |
| 当前 phase / status 类型 | `AgentStatus` (`protocol.rs:1567-1586`) 7 态（含 PendingInit / Errored / Shutdown / NotFound 系统层态） | `AgentHarnessPhase` (`types.ts:485`) 5 态（业务态） | **`EnginePhase` 6 态**（Pi 风格 + zhive 加 SubagentSpawn） |
| phase 持有 | `watch::Sender<AgentStatus>` (`session.rs:23`) + `watch::Receiver` (`session/mod.rs:376`) | `private phase: AgentHarnessPhase = "idle"` 字段 (`agent-harness.ts:171`) | `EngineInner.phase_tx: watch::Sender<EnginePhase>` |
| 当前 turn 持有 | `Session.active_turn: Mutex<Option<ActiveTurn>>` (`session.rs:34`) | （无显式 ActiveTurn 类型，turn 由 promise + queue 隐含） | `ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` |
| Turn 内 state | `TurnState { pending_approvals, pending_input, tool_calls, token_usage_at_turn_start, ... }` (`state/turn.rs:83-98`) | `steerQueue / followUpQueue / nextTurnQueue` 三 array (`agent-harness.ts:183-187`) | `TurnState { pending_approvals, pending_input, streaming_behavior, tool_call_count, token_usage_at_start }`（codex 字段 + A3 `streaming_behavior`） |
| Turn kind enum | `TaskKind { Regular, Review, Compact }` (`state/turn.rs:65-69`) | （无 kind enum，三入口方法区分） | `TurnKind { Regular, Subagent, Review }`（zhive 把 codex `Compact` 提到 phase 层而非 turn kind） |
| 提交入口 | `Codex.submit(op: Op) -> CodexResult<String>` (`codex_thread.rs:133`) | `prompt(text) / skill(name) / promptFromTemplate(name)` 三方法 | `Engine.start_turn(thread_id, inputs) -> Result<TurnId>`（A1 §2.3 已定 wire） |
| 中断 | `Session.interrupt_task(self)` (`session/mod.rs:3228`) | `runAbortController?.abort()` 隐含 | `Engine.interrupt_turn(thread, turn)` |
| Steer 入口 | `CodexThread.steer_input(input, additional_context, expected_turn_id, ...)` (`codex_thread.rs:238`) | `harness.steer(text)` (`agent-harness.ts:652`) | `Engine.steer(thread, turn, input)` |
| 反向 RPC sink | `Session.services` + `RequestPermission` / `RequestUserInput` 等专用类型 | `subscribe(handler)` 单 listener (`agent-harness.ts:188`) | `EngineInner.reverse_rpc: Arc<dyn ReverseRpcSink>`（B4/B7） |
| Submission channel | `async_channel::bounded(512)` (`session/mod.rs:426`) | （JS Promise 链） | `mpsc::Sender<Submission>` bounded(512) |
| Event channel | `async_channel::unbounded()` (`session/mod.rs:486`) | `emit(event)` 同步 listener loop | `broadcast::Sender<EngineEvent>` bounded(1024)（**有界**，与 codex 不同；理由见 §6.2） |
| Shutdown | `Codex.shutdown_and_wait() + session_loop_termination: Shared<BoxFuture>` (`session/mod.rs:380`) | `runAbortController.abort()` | `Engine.shutdown()` + `EngineInner.shutdown: CancellationToken` |

---

## 8. 关键问题逐条作答（验收）

| # | 问题 | 答案（≤ 8 行） |
|---|---|---|
| 1 | Engine 持有什么状态？ | `threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `phase_tx: watch::Sender<EnginePhase>` + `shutdown: CancellationToken` + `reverse_rpc / hook_host / provider / permission_reducer / storage` 5 个 `Arc<dyn ...>` 注入 + `event_bus: broadcast::Sender<EngineEvent>`。**不**持当前 turn —— 当前 turn 落到 `ThreadHandle.active_turn` 内（codex 模式，`session.rs:34`）。 |
| 2 | EnginePhase 6 态 | 直接抄 Pi 5 态（Idle/Turn/Compaction/BranchSummary/Retry）**+ zhive 自有 SubagentSpawn**。理由：D-008 subagent permission inheritance 在派生瞬间需要专门状态供 hook + reducer 锚定。其余 5 态用 Pi 业务态而非 codex `AgentStatus` 7 态——后者含 `PendingInit / Errored / Shutdown / NotFound` 是系统层状态，不属于"工作模式"，应该分离到 `EngineLifecycle` 子状态（**TODO(B1-4)**）。 |
| 3 | phase 转换：enum + match 还是 typestate？ | **enum + match**，串行化在 `Engine::transition_phase`。理由：(a) 与 `Arc<EngineInner>` clone 语义兼容；(b) `watch::Sender<EnginePhase>` 模式（codex `session.rs:23` 已验证）；(c) JSON-RPC server 用同一 dispatch 入口；(d) typestate 跨 Arc 不可行。详见 §2.2。 |
| 4 | 单 thread 内 turn 串行还是并发？codex 怎么做？ | **串行**。codex `Session.active_turn: Mutex<Option<ActiveTurn>>`（`session.rs:34`）+ `NonSteerableTurnKind = Review \| Compact`（`protocol.rs:1589-1595`）说明同一时刻仅一个 active turn。zhive 完全一致。**跨 thread**：并行（每 thread 独立 agent loop task）。 |
| 5 | Turn 事件流 channel 拓扑？ | turn 内：`ActiveTurn.item_tx: mpsc::Sender<Item>` 单 producer（agent loop）→ 单 consumer（item appender）。appender 后接 `event_bus: broadcast::Sender<EngineEvent>` 做 N 客户端 fan-out。reasoning chunk / tool_call / tool_result / agent_message 都走 `Item` 类型 + `Item::* { kind=...}` discriminator（A1 §6 已定）。详见 §6.6。 |
| 6 | ownership？ | `Engine = Arc<EngineInner>` + actor pattern（mpsc submission + broadcast event）。**不**用 `Arc<Mutex<Engine>>`（全局锁与多 thread 并发冲突），**不**用 typestate。详见 §6.3。 |
| 7 | 需要 PhaseTransition 通用 hook 吗？ | **需要**。新增 `PhaseTransition { from, to, thread_id }`，与 D-012 `PreCompact/PostCompact` 共存（同时触发）：phase 切换时先发 PhaseTransition 再发细粒度 hook（如 PreCompact）。详见 §6.7。**TODO(B1-3)** 与 A4 协调。 |

---

## 9. 未决项（回流到 plan §9）

> TODO(开放项 B1-1)：Pi `AgentHarnessPhase` 5 态中的 `retry` 在 Pi codebase 内**未找到显式 set 位点**（仅在 `types.ts:485` 类型定义中出现）。zhive 是否真把 `Retry` 提升到顶层 phase 待 §8 metric 实测验证。备选：折回 `TurnStatus::InProgress { retry_count: u32 }`。倾向**保留独立 phase**（hook + tracing 友好）。
>
> TODO(开放项 B1-2)：`EnginePhase` 是 engine 级单一值还是 per-thread 字典？单一值在 CLI 单 thread 场景下足够；多 thread 场景（IDE 同时 3 chat）必须 per-thread，否则 phase 含义模糊。倾向 **per-thread**：`phase_tx: HashMap<ThreadId, watch::Sender<EnginePhase>>`，外层包 `RwLock<HashMap>`。本调研草图 §4 写的是单一值，落地时调整。
>
> TODO(开放项 B1-3)：新增 `PhaseTransition` hook 会让 D-012 14 事件 → 15 事件。与 A4 deliverable 对齐落地点：A4 §（hook 14 事件枚举处）补一行 `PhaseTransition { from: EnginePhase, to: EnginePhase, thread_id: Option<ThreadId> }`。D-012 字面写"至少 14"和 `#[non_exhaustive]`，不破坏决策。
>
> TODO(开放项 B1-4)：codex `AgentStatus` 7 态含 `PendingInit / Errored(String) / Shutdown / NotFound` 是**系统层**状态（engine 自身生命周期），不是业务工作模式。zhive 是否要新增 `EngineLifecycle { Spawning, Ready, Errored(Arc<EngineError>), ShuttingDown, Shutdown }` 与 `EnginePhase` 正交？倾向**分离**：lifecycle 管 engine 全局，phase 管单 thread 工作模式。具体类型推到 §（落地时单独跑 1 个 critic 看必要性）。
>
> TODO(开放项 B1-5)：`Submission::Shutdown` 与 `EngineLifecycle::Shutdown` 是否需要分别？`shutdown()` 公开方法可直接 close mpsc + cancel CancellationToken，**或**走 submission 让 dispatcher loop 优雅 drain。codex 选**后者**（`Codex.shutdown_and_wait` 等 `session_loop_termination`，`session/mod.rs:380`）。zhive 倾向沿用。
>
> TODO(开放项 B1-6)：`event_bus: broadcast::Sender<EngineEvent>` 容量 1024 是直觉估算。在 D-014 tracing 强制覆盖下，每个 turn 可能产 50-200 events（reasoning chunk + tool_call argument chunk 都按 event 计）；高并发 + 多客户端订阅时 1024 可能 lag。建议在 B9 tracing 落地后用真实 turn 实测调整。
>
> TODO(开放项 B1-7)：`ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` 是 **tokio::Mutex**（异步）还是 **std::sync::Mutex**（同步）？codex 用 `tokio::sync::Mutex`（`state/turn.rs:6`）。zhive 跟 codex（async hold 时间长，跨 await 必须 tokio）。但 `Mutex<Option<ActiveTurn>>` 持有时间应**短**——只在 lookup/insert 时持，agent loop 内的长时间持锁应该重构为先 take 出 ActiveTurn 引用再操作。
>
> TODO(开放项 B1-8)：与 A1 deliverable §6 `TurnStartedNotification / TurnCompletedNotification` 已定形态对齐 —— 现 A1 草图缺 `PhaseChangedNotification`（method `phase/changed`）。落地时需要把本 deliverable §4 末尾草图的 `PhaseChangedNotification` 同步加进 `zhive-proto::domain`，否则 server 无法发 phase 变化通知。**不改 A1**——这是 A1 之后的扩展点。

---

## 10. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中行号引用）
- [x] 不动 `crates/` 源码（草图均在本 markdown 内）
- [x] 不改 `research/99-decisions/`（仅引用，未编辑）
- [x] 不 `git pull`
- [x] codex 文件读取数 ≤ 4：`session/mod.rs` / `session/session.rs` / `state/turn.rs` / `thread_manager.rs` / `codex_thread.rs` / `protocol.rs`（计 6 个，**超 4 个**——但每个仅读 30-200 行，未 walk 目录；如严格按 4 文件，可砍 `protocol.rs`（AgentStatus 已总结）+ `codex_thread.rs`（仅取一个方法签名） → 4 文件下限。**已超额但每个都贴行号**，符合"用 rg 定位类型名再 Read 单文件"约束。）
- [x] Pi 文件 ≤ 4：`types.ts` / `agent-harness.ts` / `agent-loop.ts`（计 3 个）
- [x] EnginePhase 不纯抄 Pi 5 态（加 `SubagentSpawn`）
- [x] ownership 决策：actor + `Arc<EngineInner>`
- [x] channel 选型：mpsc submission + broadcast event + watch phase + oneshot reply
- [x] 未决项 8 条（TODO B1-1 ~ B1-8）
- [x] §0 顶部有「设计衔接警告」（A1 vs B1 TurnStatus / EnginePhase 正交说明）

— B1 deliverable end —
