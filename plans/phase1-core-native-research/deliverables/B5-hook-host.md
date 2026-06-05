---
task: B5 — Hook host（D-012 host 侧 + 红线 10/11）
plan: phase1-core-native-research
date: 2026-05-28
status: implemented
owner: B5 subagent
depends_on:
  - A4 deliverable（HookEvent 14 case + HookEventBase + ExtensionRef + Unknown 兜底）
  - A5 deliverable（manifest = hook 唯一注册通道；ExtensionScope + HookHandle + Drop 兜底）
  - B1 deliverable（broadcast(1024) event 总线 + watch::Receiver<EnginePhase> + actor pattern）
  - 红线 10（每个注册 hook 必带 registered_by: ExtensionRef）
  - 红线 11（tool_call mutate event.input 后 host 必须再过一次 schema 验证）
consumed_by:
  - B1 EngineInner.hook_host: Arc<dyn HookHost>
  - B6 Permission reducer（PreToolUse → PermissionDecision 折叠）
---

# B5 · Hook host deliverable

> 决策冲突警告 1：Claude Code 文档（[hooks 参考](https://code.claude.com/docs/en/hooks)）明确"all matching hooks run in **parallel**, identical handlers are deduplicated"。Pi `runner.ts:680-712` 是**串行** for-of。zhive 本 deliverable 选 **串行 + manifest 显式 priority 排序**（详 §3.3）—— 与 Claude Code 文档分歧，理由：mutate 后重验证（红线 11）要求"前一个 hook 的输出是后一个 hook 的输入"，并行 mutate 是数据竞争。建议在 `decision-diffs.md` 记一条说明分歧来源。

> 决策冲突警告 2：A4 deliverable §6 列 `PreToolUse` 是 D-012 14 之一（zhive 独有，Pi 无对应），等价 Pi 的 `tool_call`（`types.ts:816-830`）但**强制 mutate 后重验证**（红线 11）。本 deliverable §6 给出"重验证失败 → abort turn"选型，不回滚到 mutate 前——理由见 §6.2。

> 决策冲突警告 3：B1 deliverable §6.7 提议新增 `PhaseTransition` hook（事件数 14→15）。本 deliverable 不在 14 case 表里独立列 `PhaseTransition`，但在 §8 对照表加了一行 "PhaseTransition (B1 提议)" 标 ⚠️ 待 A4 / D-012 修订决定。

---

## 0. 摘要

- **执行模型**：双轨——**进程内**（in-process `dyn HookFn`，零序列化代价）+ **子进程**（外部程序走 stdin JSON / stdout JSON 协议）。理由：builtin hook 是 Rust 函数，进程内 trait 调用零 IPC 开销；子进程轨服务 manifest 字段 `entrypoint: cmd:...` 形态的外部 hook。`HookExecutor` enum 有 `InProcess(Arc<dyn HookFn>)` + `Subprocess(Arc<SubprocessSpec>)` 两 variant，均已落地（`run_subprocess_hook` + `register_subprocess_hook`）。
- **注册时机**：**startup 一次性扫盘**（manifest 扫盘走 A5 §4 流程），**配合手动 `/reload`**（A5 §7.2 决定 A）；不做 fs-watch。
- **顺序策略**：`(extension_source_rank, manifest_priority, registration_order)` 三键 lex order。settingSources rank 与 A5 §3 三层（user > project > local）+ builtin > mcp 对齐；priority 是 manifest `[[hooks]] priority = N` 整数字段（默认 0）。
- **错误隔离**：每个 hook 一个 `tokio::time::timeout` + `tokio::spawn_blocking`（如 InProcess sync fn）/ `catch_unwind`-shim；单 hook 失败 / panic / timeout 不抛出 turn pipeline，写入 `HookExecutionError` 上报 D-014 tracing，dispatch 继续下一个 hook。
- **红线 10 落地**：`register_hook` 签名强制收 `extension_ref: ExtensionRef`（非 Option），host 在挂表前自动把它 stamp 进 `HookEventBase.registered_by`；builtin hook 由 host 内置 `ExtensionRef::builtin("zhive-core", env!("CARGO_PKG_VERSION"))` 自动填。
- **红线 11 落地**：`PreToolUse` 是唯一允许 mutate `tool_input` 的 event；每次 mutate 后 host 立即对 mutate 后的 `tool_input` 跑 JSON Schema 验证（schema 来自 A5 `parameters_schema`），失败 → **abort turn**（不回滚到 mutate 前），原因：mutate-rollback 在 hook chain 中会丢失对中间 hook 副作用的可观测性，且与"hook 失败 abort turn"一致；ABORT 走 D-014 tracing + 写一条 `Item::HookValidationError` 进 transcript。
- **pending queue 回滚**：`VecDeque<UserInput>` + `splice` 语义；失败时 `unshift_front`（push_front 反向插入回去）—— 1:1 对齐 Pi `agent-harness.ts:391-401` 的 `queue.unshift(...messages)`。
- **zombie listener 防护**：A5 §7.2 `ExtensionScope` + `HookHandle` + `Drop` 兜底，本 deliverable §7 给出 host 端 `HashMap<HookHandle, RegisteredHook>` 的具体类型 + `unregister_scope` 算法。

---

## 1. 参考点清单

### 1.1 zhive 内部

| 路径 | 行号 | 用途 |
|---|---|---|
| `plans/phase1-core-native-research/deliverables/A4-hook-event-schema.md` | 全文 | HookEvent 14 case + HookEventBase + ExtensionRef + Unknown 兜底；本 deliverable 的 schema 输入 |
| `plans/phase1-core-native-research/deliverables/A5-extension-manifest.md` | §4, §7.2 | hook 注册唯一通道 = manifest；ExtensionScope + HookHandle |
| `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | §6.2 | broadcast(1024) + watch + actor pattern；EngineInner.hook_host: Arc<dyn HookHost> |
| `plans/phase1-core-native-research/phase1-core-native-research.md` | L386-418 | B5 任务定义 |
| `research/99-decisions/README.md` | L317-338, 435-437 | D-012 / 红线 10 / 红线 11 原文 |

### 1.2 Pi（每条 ≤ 4 文件）

| 路径 | 行号 | 用途 |
|---|---|---|
| `${PI}/packages/agent/src/harness/agent-harness.ts` | 391-401 | `drainQueuedMessages` 失败 `queue.unshift(...messages)` 回滚语义（pending queue 回滚锚点）|
| `${PI}/packages/agent/src/harness/agent-harness.ts` | 416-424 | `beforeToolCall` emitHook → `{ block, reason }` 形态（PreToolUse host 调度面貌锚点）|
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 816-830 | `tool_call` mutate `event.input` **未重验证 schema 反例**（红线 11 反例锚点）|
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 984-988 | `ToolCallEventResult { block?, reason? }`（mutate 只对 input，结果靠 block 决策）|
| `${PI}/packages/coding-agent/src/core/extensions/runner.ts` | 680-712 | `emit()` 串行 for-of + try/catch 单 hook 隔离（错误隔离参考实现）|

### 1.3 外部文档

| URL | 用途 |
|---|---|
| https://code.claude.com/docs/en/hooks | PreToolUse decision shape、timeout（command/http/mcp_tool 默认 600s，prompt 30s，agent 60s）、matcher / parallel dedup 语义、`continue: false` 全局 abort |
| https://code.claude.com/docs/en/agent-sdk/hooks | TS SDK callback chain（每个 callback 一个 hook event type，await 序列化） |

---

## 2. `HookHost` trait + 内置实现草图

```rust
//! B5 落地。HookHost trait + 默认实现 + ExtensionScope 配合。
//! crates/zhive-host/src/hook/ ——草图，不在本 deliverable 周期内落码。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use zhive_proto::hook::{ExtensionRef, HookEvent, HookEventBase, PreToolUseInput};

// ─────────────────────────────────────────────────────────────────────────
// HookHandle / ExtensionScope（与 A5 §7.2 对齐）
// ─────────────────────────────────────────────────────────────────────────

/// host 端 hook 句柄。**opaque**：extension 只能用 host API 操作它，不能裸建。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookHandle(u64);

/// 单 extension 持有的所有 hook handle 容器。reload 时按 extension_id 整体撤销。
pub struct ExtensionScope {
    extension_id: ExtensionId,
    handles: Vec<HookHandle>,
    host: std::sync::Weak<dyn HookHost + Send + Sync>,
}

impl Drop for ExtensionScope {
    fn drop(&mut self) {
        // 兜底：异常路径（panic / extension 提前 drop）时仍主动撤销 listener。
        // 正常 reload 走 host.unregister_scope() 显式撤销，到这里时 handles 已空。
        if let Some(host) = self.host.upgrade() {
            for h in self.handles.drain(..) {
                let _ = host.unregister_one(h);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub String);  // e.g. "git-helper@0.1.0"

// ─────────────────────────────────────────────────────────────────────────
// 事件 kind discriminator（避免拿整个 HookEvent struct 当 key）
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEventKind {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PermissionRequest,
    Stop,
    Notification,
    Setup,
    ToolApprovalChange,
    // Unknown 不可订阅（A4 §5 决策）
}

// ─────────────────────────────────────────────────────────────────────────
// Hook fn 签名 + Decision
// ─────────────────────────────────────────────────────────────────────────

/// hook 返回的决策。对齐 Claude Code 文档 `hookSpecificOutput`/`continue` 字段：
/// - `Continue`：默认；流程继续
/// - `BlockAction { reason }`：拒绝当前动作（PreToolUse → 拒 tool call；PermissionRequest → deny）
/// - `AbortTurn { reason }`：等价 `continue: false`，整 turn 终止
/// - `MutateInput { new_input }`：仅 PreToolUse 合法；host 必须重验证 schema（红线 11）
#[derive(Debug, Clone)]
pub enum HookDecision {
    Continue,
    BlockAction { reason: String },
    AbortTurn { reason: String },
    /// Pi `event.input` 原地 mutate 的 Rust 化：显式 new_input 而非内存可变引用，
    /// 避免 Send + 并发 borrow 风险。host 拿到后跑 §6 重验证流程。
    MutateInput { new_input: Value },
}

/// hook handler signature。
/// - 选 Box<dyn ... + Send + Sync> 而非 Arc：每个 handler 唯一 owner（host 端 HashMap）；reload 时 drop 即释放。
/// - 选 Pin<Box<dyn Future>>（async fn 等价）：B1 内 tokio runtime 已成基础设施。
pub type BoxedHookFn = Box<
    dyn Fn(HookEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HookDecision, HookFnError>> + Send>>
        + Send
        + Sync
        + 'static,
>;

#[derive(Debug, Error)]
pub enum HookFnError {
    #[error("hook handler panicked: {0}")]
    Panic(String),
    #[error("hook handler returned error: {0}")]
    Logic(#[source] Box<dyn std::error::Error + Send + Sync>),
}

// ─────────────────────────────────────────────────────────────────────────
// host 内部表项
// ─────────────────────────────────────────────────────────────────────────

struct RegisteredHook {
    handle: HookHandle,
    event_kind: HookEventKind,
    extension_ref: ExtensionRef,   // 红线 10：必填，由 register_hook 强制
    extension_id: ExtensionId,
    /// (source_rank, priority, registration_seq) —— 三键排序
    sort_key: (u8, i32, u64),
    timeout: Duration,             // 来自 manifest 字段，默认 30s（对齐 Claude Code UserPromptSubmit 默认）
    executor: HookExecutor,
}

/// 双轨：InProcess（Rust 闭包）+ Subprocess（外部程序，stdin/stdout JSON 协议），均已落地。
enum HookExecutor {
    InProcess(Arc<dyn HookFn>),
    /// manifest 写 `entrypoint = "cmd:./main.sh"` 时走这条。
    Subprocess(Arc<SubprocessSpec>),
}

// ─────────────────────────────────────────────────────────────────────────
// HookHost trait —— B1 EngineInner.hook_host: Arc<dyn HookHost> 的形态
// ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait HookHost: Send + Sync {
    /// 红线 10 enforce 点：extension_ref 是必填参数，不是 builder option。
    fn register_hook(
        &self,
        scope: &mut ExtensionScope,
        event_kind: HookEventKind,
        extension_ref: ExtensionRef,
        priority: i32,
        timeout: Duration,
        handler: BoxedHookFn,
    ) -> Result<HookHandle, HookHostError>;

    /// reload 主路径（A5 §7.2 决定 D step 2）。
    fn unregister_scope(&self, extension_id: &ExtensionId);

    /// Drop 兜底用，单条撤销。
    fn unregister_one(&self, handle: HookHandle) -> Result<(), HookHostError>;

    /// 给 Engine 主路径用：dispatch 一个事件，按 sort_key 顺序串行调用所有匹配 hook。
    /// 返回 fold 后的最终决策（详 §3.2 reducer）。
    async fn dispatch(
        &self,
        event: HookEvent,
        cancel: CancellationToken,
    ) -> Result<DispatchOutcome, HookHostError>;
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// 所有 hook 都 Continue。
    Continue,
    /// 至少一个 hook BlockAction（Engine 决定是否拒动作 —— PreToolUse → 拒 tool；
    /// PermissionRequest → deny；其它 event 该结果由 B6 reducer 解读）。
    Blocked { reason: String, by: ExtensionRef },
    /// 至少一个 hook AbortTurn（B1 engine loop 必须切 phase → Idle 并清 pending）。
    Aborted { reason: String, by: ExtensionRef },
    /// PreToolUse 链 mutate 后的最终 input（已通过 schema 重验证）。
    MutatedInput { final_input: Value, mutators: Vec<ExtensionRef> },
}

#[derive(Debug, Error)]
pub enum HookHostError {
    #[error("unknown hook handle")]
    UnknownHandle,
    #[error("hook validation failed after mutate: {0}")]
    SchemaRevalidationFailed(String),
    #[error("hook execution error: {0}")]
    Execution(#[source] HookFnError),
    #[error("hook timeout after {0:?}")]
    Timeout(Duration),
    #[error("hook host poisoned")]
    Poisoned,
}

// ─────────────────────────────────────────────────────────────────────────
// 默认实现（草图）
// ─────────────────────────────────────────────────────────────────────────

pub struct DefaultHookHost {
    inner: Arc<RwLock<HookHostInner>>,
    next_handle: std::sync::atomic::AtomicU64,
    /// tool_input schema 注册表（来自 A5 manifest `parameters_schema`）
    tool_schemas: Arc<RwLock<HashMap<String, jsonschema::JSONSchema>>>,
}

struct HookHostInner {
    /// 按 event_kind 索引；Vec 内已按 sort_key 升序，dispatch O(N) 不 sort
    by_event: HashMap<HookEventKind, Vec<HookHandle>>,
    /// handle → 真正条目
    table: HashMap<HookHandle, RegisteredHook>,
    /// extension_id → 其所有 handles（unregister_scope 用）
    by_extension: HashMap<ExtensionId, Vec<HookHandle>>,
}
```

> TODO(B5-1)：`BoxedHookFn` 是否需要支持 `&mut state` capture？Phase 1 builtin hook 多为无状态，先用 `Fn`；如果出现需要可变状态的内置 hook（如 turn-counter），改为内部 `Arc<Mutex<State>>` 闭包捕获，不动签名。

> TODO(B5-2)：A5 已用 `schemars` 出 schema，校验侧用 workspace dep `jsonschema = "0.46"`（已落地）。

---

## 3. 注册 / 调度 / 错误隔离的状态机

### 3.1 状态机（host 侧 hook 生命周期）

```text
                  manifest 扫盘（A5 §4）
                            │
                            ▼
                ┌────────────────────────┐
                │ Discovered（候选 hook） │
                └──────────┬─────────────┘
                           │  register_hook(scope, kind, ext_ref, prio, timeout, fn)
                           │  │ 红线 10：ext_ref 必填校验
                           │  │ 若 fail → reject（不进表）
                           ▼
                ┌────────────────────────┐
                │ Registered              │──── dispatch 命中 ────┐
                └──────────┬─────────────┘                        │
                           │                                       ▼
                           │                            ┌──────────────────────┐
              unregister_  │                            │ Executing            │
              scope(ext_id)│                            │ - tokio::time::timeout│
                           │           ┌────────────────│ - catch_unwind shim   │
                           │           │                │ - cancel token race   │
                           ▼           │                └───┬───────┬───────┬───┘
                ┌────────────────────────┐                  │       │       │
                │ Tombstoned（已撤）      │◄─────────────────┘       │       │
                └────────────────────────┘     ok                     │       │
                                                  ┌───────────────────┘       │
                                                  ▼                            │
                                          fold 进 DispatchOutcome              │
                                                                               │
                                                              err / timeout / panic
                                                                               │
                                                                               ▼
                                                                    HookExecutionError →
                                                                    tracing::warn + 跳过此 hook
                                                                    （不影响后续 hook 执行）
```

### 3.2 dispatch 算法（reducer 形态）

```rust
// 伪码，对齐 Pi runner.ts:680-712 但加红线 11 重验证 + 顺序 fold
async fn dispatch(&self, event: HookEvent, cancel: CancellationToken)
    -> Result<DispatchOutcome, HookHostError>
{
    let kind = event.kind();
    let table = self.inner.read();
    let Some(handles) = table.by_event.get(&kind) else {
        return Ok(DispatchOutcome::Continue);
    };

    // PreToolUse 走 mutate 链；其它 event 走"BlockAction / AbortTurn 短路"链
    let mut current_event = event.clone();
    let mut mutators: Vec<ExtensionRef> = Vec::new();

    for h in handles {  // handles 已按 sort_key 升序
        let hook = &table.table[h];
        // 两轨统一成一个 future：进程内直接调闭包，子进程走 run_subprocess_hook
        let fut = match &hook.executor {
            HookExecutor::InProcess(f) => f.call(current_event.clone()),
            HookExecutor::Subprocess(spec) => {
                run_subprocess_hook_boxed(spec.clone(), current_event.clone())
            }
        };

        // 错误隔离 + timeout + cancel race
        let res = tokio::select! {
            _ = cancel.cancelled() => return Ok(DispatchOutcome::Aborted {
                reason: "cancelled by turn-level cancel".into(),
                by: hook.extension_ref.clone(),
            }),
            r = tokio::time::timeout(hook.timeout, fut) => r,
        };

        let decision = match res {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                tracing::warn!(
                    extension = %hook.extension_ref.id,
                    event = ?kind,
                    error = %e,
                    "hook handler errored; isolating and continuing"
                );
                continue;  // 错误隔离：跳过此 hook，不抛进 turn
            }
            Err(_elapsed) => {
                tracing::warn!(
                    extension = %hook.extension_ref.id,
                    timeout_ms = hook.timeout.as_millis() as u64,
                    "hook timed out; isolating and continuing"
                );
                continue;
            }
        };

        match decision {
            HookDecision::Continue => {}
            HookDecision::BlockAction { reason } => {
                return Ok(DispatchOutcome::Blocked {
                    reason,
                    by: hook.extension_ref.clone(),
                });  // 短路
            }
            HookDecision::AbortTurn { reason } => {
                return Ok(DispatchOutcome::Aborted {
                    reason,
                    by: hook.extension_ref.clone(),
                });  // 短路
            }
            HookDecision::MutateInput { new_input } => {
                if !matches!(kind, HookEventKind::PreToolUse) {
                    // 红线 11 衍生约束：只有 PreToolUse 允许 mutate
                    tracing::warn!(event = ?kind, "MutateInput on non-PreToolUse event ignored");
                    continue;
                }
                // 红线 11：mutate 后立即重验证 schema
                current_event = self.apply_mutate_and_revalidate(
                    current_event,
                    new_input,
                    &hook.extension_ref,
                )?;  // 失败 → SchemaRevalidationFailed → engine 把 turn abort
                mutators.push(hook.extension_ref.clone());
            }
        }
    }

    if mutators.is_empty() {
        Ok(DispatchOutcome::Continue)
    } else if let HookEvent::PreToolUse(p) = current_event {
        Ok(DispatchOutcome::MutatedInput {
            final_input: p.tool_input,
            mutators,
        })
    } else {
        unreachable!("mutators non-empty implies PreToolUse")
    }
}
```

### 3.3 顺序策略：`(source_rank, priority, registration_order)`

| 维度 | 取值 | 来源 |
|---|---|---|
| `source_rank: u8` | `Builtin=0, User=1, Project=2, Local=3, Mcp=4`（数值越小越先跑）| A4 §2 `ExtensionSource` + A5 §3 settingSources |
| `priority: i32` | 默认 0；manifest 可显式写负数 / 正数 | A5 manifest `[[hooks]] priority = N` |
| `registration_order: u64` | 注册时的单调 atomic（同 source / priority 时按到达顺序）| host 内部 `AtomicU64::fetch_add` |

**为何不用 namespace**：Pi 把 hook 与 extension namespace 紧耦合（`ExtensionAPI.on(event, handler)` `types.ts:1089-1126`），namespace 即 extension id。zhive 同形态——`extension_ref.id` 已含 namespace，无需另立维度。

**为何不并行（与 Claude Code 文档分歧）**：见 §0 警告 1 与 §6.3。

### 3.4 错误隔离方案

| 失败类型 | 隔离手段 | 是否打断 dispatch |
|---|---|---|
| handler return `Err(HookFnError::Logic)` | `tracing::warn!` 记录 + 跳过此 hook | ❌ 继续下一个 |
| handler `panic!` | `catch_unwind` shim（注册时套一层 `FutureExt::catch_unwind`）→ `HookFnError::Panic` | ❌ 继续下一个 |
| `tokio::time::timeout` 超时 | `tracing::warn!` + 跳过 | ❌ 继续下一个 |
| `cancel.cancelled()`（turn-level cancel） | 返回 `DispatchOutcome::Aborted` | ✅ 立刻退出 |
| 红线 11 schema 重验证失败 | `HookHostError::SchemaRevalidationFailed` 抛回 Engine | ✅ Engine 决定 abort turn |
| handler 返回 `BlockAction` / `AbortTurn` | 短路返回 | ✅（按 Decision 类型） |

**为何 panic 也只隔离不抛**：tokio task panic 会污染 runtime（log noise），通过 `catch_unwind` 把 panic 转 Error，对齐 Pi `runner.ts:698-706` `try/catch` + `emitError` 的兜底行为，但用 Rust idiom 实现（不是裸 unwind）。

> TODO(B5-3)：`catch_unwind` 对 async closure 的支持需要 `FutureExt::catch_unwind`（`futures` crate 已在 workspace？若否走 cargo add）。或者手写 `AssertUnwindSafe` wrap —— 决定推到实装期。

---

## 4. `ExtensionRef` + `register_hook` API（红线 10 enforce）

### 4.1 ExtensionRef 结构（A4 已定，本 deliverable 复述）

```rust
// 来自 A4 deliverable §3，B5 host 直接 import
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub struct ExtensionRef {
    pub id: String,           // 全局唯一，e.g. "builtin:filesystem-guard" / "user:my-skill"
    pub version: String,      // semver
    pub source: ExtensionSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    Builtin,
    User,
    Project,
    Local,
    Mcp,
}

impl ExtensionRef {
    /// builtin hook 注册用的 helper；version 自动取 zhive 自身 cargo pkg version。
    pub fn builtin(name: &str) -> Self {
        Self {
            id: format!("builtin:{name}"),
            version: env!("CARGO_PKG_VERSION").to_string(),
            source: ExtensionSource::Builtin,
        }
    }
}
```

### 4.2 register_hook 签名（enforce 路径）

```rust
fn register_hook(
    &self,
    scope: &mut ExtensionScope,
    event_kind: HookEventKind,
    extension_ref: ExtensionRef,    // ← 红线 10：必填位置参数，不是 Option / builder
    priority: i32,
    timeout: Duration,
    handler: BoxedHookFn,
) -> Result<HookHandle, HookHostError>;
```

**enforce 形态**：
1. 位置参数（不是 Option / 不是 builder pattern with `set_extension_ref`），编译期就不能省略
2. 类型 `ExtensionRef` 而非 `Option<ExtensionRef>`，调用者必须构造
3. host 在挂表后，dispatch 时**强制**把 `extension_ref` clone 进 `HookEvent` 的 `HookEventBase.registered_by` 字段（覆盖 caller 传入值，避免 caller 伪造他人身份）
4. manifest 解析期（A5 loader 路径）host 用 manifest 的 `[extension]` section 推出 `ExtensionRef` 后再调 `register_hook`，extension 自己**永远不能**手动构造 `ExtensionRef::user(...)` 之类（A5 §3 `kind = extension | prompt | skill` 三 namespace 决定 id 前缀）

**伪造防护**：`ExtensionRef::user / project / local / mcp` constructor **不向 extension API 暴露**（仅 host crate `pub(crate)`）；`builtin` constructor 也仅 host 内部用。manifest 解析后由 host 端唯一注入。

```rust
// 给 builtin hook 用的安全注册 helper（zhive 自己的 src 内）
impl DefaultHookHost {
    pub(crate) fn register_builtin(
        &self,
        scope: &mut ExtensionScope,
        name: &str,
        kind: HookEventKind,
        handler: BoxedHookFn,
    ) -> Result<HookHandle, HookHostError> {
        self.register_hook(
            scope,
            kind,
            ExtensionRef::builtin(name),  // 唯一暴露的 ExtensionRef constructor
            0,  // 默认 priority
            Duration::from_secs(30),
            handler,
        )
    }
}
```

### 4.3 stamp 机制（dispatch 时填充 registered_by）

host 在 dispatch 的每次 hook 调用前，clone 当前 event 并把 `event.base_mut().registered_by = hook.extension_ref.clone()`。这样：
- 每个 hook 接收到的 event payload **始终**带正确的 registered_by（不是注册者，是当前正在处理这个 event 的 hook 的归属）
- 同一原始 event 在 hook 链中流转时，每个 hook 看到的 registered_by 都是自己 —— 用于 hook 内部审计 / 自我识别（"是不是别人在 invoke 我"）

> TODO(B5-4)：A4 `HookEventBase` 字段 immutable by `#[derive(Deserialize)]`；要么改 `pub registered_by: ExtensionRef` 让 host 写它，要么 host 端用 `HookEvent::with_registered_by(&mut self, ref)` 显式 setter —— 推到 A4 deliverable 微调或 B5 实装期决定。

---

## 5. Pending queue 回滚机制（Pi `drainQueuedMessages` Rust 化）

### 5.1 Pi 锚点

`${PI}/packages/agent/src/harness/agent-harness.ts:391-401`:

```ts
private async drainQueuedMessages(queue: AgentMessage[], mode: QueueMode): Promise<AgentMessage[]> {
    const messages = mode === "all" ? queue.splice(0) : queue.splice(0, 1);
    if (messages.length === 0) return messages;
    try {
        await this.emitQueueUpdate();
        return messages;
    } catch (error) {
        queue.unshift(...messages);   // ← 失败时把 splice 出去的塞回队头
        throw normalizeHookError(error);
    }
}
```

语义：从队列前端取 N 个，**尝试**处理（这里是 emit hook）；处理失败 → 原样塞回队头 + 抛出错误，**保证队列不丢消息**。

### 5.2 zhive 数据结构选型：`VecDeque<UserInput>`

| 候选 | 适合度 | 拒因 |
|---|---|---|
| `Vec<T>` + `drain(0..n)` + `splice` 回填 | ⚠️ | drain 后回填需要 `vec.splice(0..0, ...)`，O(N) move；语义不如 VecDeque 直接 |
| **`VecDeque<T>`** + `drain(0..n)` + `extend_front` | ✅ | front 操作 O(1) amortized；语义直接对应 unshift |
| `LinkedList<T>` | ❌ | std 不鼓励；分配开销大 |
| `async_channel::Receiver` | ❌ | channel 一旦 recv 出来就不能"塞回"，需要中间缓存 |

```rust
use std::collections::VecDeque;

/// turn-scoped pending input queue（A3 §StreamingBehavior + B1 ActiveTurn 内部状态）。
pub struct PendingQueue {
    inner: VecDeque<UserInput>,
}

impl PendingQueue {
    /// 取出前 N 个并尝试用 f 处理；f 失败时把它们塞回队头并返回 Err。
    /// 1:1 对齐 Pi `drainQueuedMessages` 语义。
    pub async fn drain_and_try<F, Fut, E>(
        &mut self,
        count: usize,
        f: F,
    ) -> Result<Vec<UserInput>, E>
    where
        F: FnOnce(Vec<UserInput>) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<UserInput>, E>>,
    {
        let n = count.min(self.inner.len());
        if n == 0 {
            return Ok(Vec::new());
        }
        // splice 等价：drain 0..n 拿到 Vec
        let snapshot: Vec<UserInput> = self.inner.drain(0..n).collect();

        match f(snapshot.clone()).await {
            Ok(out) => Ok(out),
            Err(e) => {
                // 失败回滚：unshift 语义 = push_front 反向插入
                for item in snapshot.into_iter().rev() {
                    self.inner.push_front(item);
                }
                Err(e)
            }
        }
    }
}
```

### 5.3 与 hook 失败的衔接

```rust
// B1 ActiveTurn 处理 pending_input 时（drain 前后由 PreToolUse / followUp / steer 三种 path 触发）
match active_turn.pending.drain_and_try(usize::MAX, |inputs| async {
    // 这里调 hook_host.dispatch(...) 把 inputs 投给 UserPromptSubmit hook 链
    let outcome = hook_host.dispatch(
        HookEvent::UserPromptSubmit(/* ...inputs... */),
        cancel.clone(),
    ).await?;
    match outcome {
        DispatchOutcome::Aborted { reason, .. } => Err(EngineError::HookAborted(reason)),
        DispatchOutcome::Blocked { reason, .. } => Err(EngineError::HookBlocked(reason)),
        _ => Ok(inputs),
    }
}).await {
    Ok(inputs) => /* 继续 turn pipeline，把 inputs 喂给 LLM */,
    Err(EngineError::HookBlocked(_)) => /* 队列已自动回滚；通知 client + 等下一轮 */,
    Err(EngineError::HookAborted(_)) => /* 队列已回滚；engine phase → Idle */,
    Err(other) => /* 同上，错误上报 */,
}
```

### 5.4 与 broadcast(1024) 总线的关系

`drain_and_try` 仅管 **turn-scoped pending queue**（A3 `TurnState.pending_input`，B1 §6.2 表）。B1 §6.2 的 `event_bus: broadcast::Sender<EngineEvent>` 是 **post-commit fan-out**（item 已落 storage 后 emit），不需要回滚——broadcast 一旦 send 出去 client 已收到。

---

## 6. 红线 11：mutate 后重验证 schema 的强制流程

### 6.1 流程图

```text
PreToolUse hook 返回 MutateInput { new_input }
            │
            ▼
   ┌──────────────────────────────────┐
   │ 1. lookup tool_schemas[tool_name]│
   │    （A5 manifest parameters_schema）│
   └──────────┬───────────────────────┘
              │
        schema 不存在
              │── 这是 Phase 1 builtin 工具未注册 schema 的 bug
              │   → SchemaRevalidationFailed("no schema for tool xxx")
              ▼
   ┌──────────────────────────────────┐
   │ 2. jsonschema::validate(&new_input)│
   └──────────┬───────────────────────┘
              │
        valid? ─── No ──────┐
              │              │
             Yes             ▼
              │     Err(SchemaRevalidationFailed { details, by: ext_ref })
              ▼              │
   ┌──────────────────────┐  │
   │ 3. 把 new_input 写回  │  ▼
   │   current_event.    │ Engine 把 turn abort（§6.2）
   │   tool_input        │ tracing::error! + 写 Item::HookValidationError
   │ 4. push mutators     │ 进 transcript（B3 JSONL）
   │ 5. 继续下一个 hook    │
   └──────────────────────┘
```

### 6.2 选型：**失败时 abort turn，不回滚到 mutate 前**

| 选项 | 优点 | 缺点 | 决定 |
|---|---|---|---|
| A. **失败 → abort turn** | 一致性：与"hook AbortTurn 返回值"语义统一；user 一定看到 turn 终止与 error item，可见性强 | turn 投入的 LLM 调用 / 上下文丢失 | ✅ |
| B. 失败 → 回滚到 mutate 前的 input，继续 | 鲁棒：单 hook bug 不破 turn | 损失中间 hook 的副作用观测；rollback 语义复杂（哪些副作用要撤？）| ❌ |
| C. 失败 → 跳过此 mutate 继续下一个 hook | 最宽容 | 让 invalid mutate "悄悄消失"，调试地狱 | ❌ |
| D. 失败 → 把 invalid input 也喂给后续 hook，让它们看错的 input | 调试友好 | 违反 schema 不变量，下游 hook 可能崩 | ❌ |

**为何选 A**：
- 红线 11 字面要求"必须再过一次 schema 验证"，但没规定失败处置；从一致性出发 abort 与"hook 直接返 AbortTurn"等价
- 跟 Claude Code `continue: false` 全局 abort 语义对齐
- 失败原因（哪个 ext / 哪条字段不符）写进 transcript 用户可读

### 6.3 串行 vs 并行（Claude Code 文档分歧补强）

Claude Code 文档说"all matching hooks run in parallel"。zhive 串行的理由：

1. **mutate 必须串行**：若两个 PreToolUse hook 同时返回 MutateInput，并行下"哪个 new_input 是最终值"无定义。Claude Code 文档实际上没说 mutate 怎么处理（它的 PreToolUse 不支持 mutate input —— 只有 `permissionDecision: allow/deny/ask/defer`）。zhive A4 比 Claude Code 多 mutate 能力（红线 11），所以**zhive 的 PreToolUse 不能并行**
2. **非 mutate event 仍串行**：保留一致心智模型；性能损失忽略（hook chain 通常 ≤ 5 个 handler，30s timeout 串行也几乎不阻塞）
3. **dedup 由 manifest 层处理**：A5 决定 hook 必须挂 manifest，同 extension 不能重复注册同 event；跨 extension 不 dedup（语义不同）

> TODO(B5-5)：若 Phase 2 出现需要并行的 event（如纯 read-only `Notification` 通知 N 个监听器），考虑加 `[[hooks]] mode = "parallel"` manifest 字段，host 端按 mode 分桶执行。

### 6.4 应用 mutate + 重验证 API 草图

```rust
impl DefaultHookHost {
    fn apply_mutate_and_revalidate(
        &self,
        event: HookEvent,
        new_input: Value,
        by: &ExtensionRef,
    ) -> Result<HookEvent, HookHostError> {
        let HookEvent::PreToolUse(mut p) = event else {
            return Err(HookHostError::SchemaRevalidationFailed(
                format!("MutateInput on non-PreToolUse from {}", by.id)
            ));
        };
        let schemas = self.tool_schemas.read();
        let Some(schema) = schemas.get(&p.tool_name) else {
            return Err(HookHostError::SchemaRevalidationFailed(
                format!("no schema registered for tool '{}'", p.tool_name)
            ));
        };
        // 红线 11：mutate 后必须验证
        if let Err(errors) = schema.validate(&new_input) {
            let details: Vec<String> = errors.map(|e| e.to_string()).collect();
            return Err(HookHostError::SchemaRevalidationFailed(
                format!("tool '{}' input invalid after mutate by {}: {}",
                    p.tool_name, by.id, details.join("; "))
            ));
        }
        p.tool_input = new_input;
        Ok(HookEvent::PreToolUse(p))
    }
}
```

---

## 7. Zombie listener 防护方案（A5 §7.2 衔接）

### 7.1 与 A5 决定的对接点

A5 §7.2 决定 B/C/D：
- 决定 B：scope token 而非 `Weak<dyn HookFn>`
- 决定 C：`ExtensionScope` + `HookHandle` 类型
- 决定 D：reload 事件序 `emit Shutdown → unregister_scope → drop → load new → emit Start`

B5 host 提供这套机制的 **服务端实现**：

```rust
impl HookHost for DefaultHookHost {
    fn unregister_scope(&self, extension_id: &ExtensionId) {
        let mut inner = self.inner.write();
        let Some(handles) = inner.by_extension.remove(extension_id) else { return };
        for h in handles {
            let Some(hook) = inner.table.remove(&h) else { continue };
            // 从 by_event 索引剔除（保持已排序状态）
            if let Some(list) = inner.by_event.get_mut(&hook.event_kind) {
                list.retain(|x| *x != h);
            }
            // hook.executor.BoxedHookFn 此时 drop —— extension 持有的闭包资源释放
        }
    }

    fn unregister_one(&self, handle: HookHandle) -> Result<(), HookHostError> {
        let mut inner = self.inner.write();
        let hook = inner.table.remove(&handle).ok_or(HookHostError::UnknownHandle)?;
        if let Some(list) = inner.by_event.get_mut(&hook.event_kind) {
            list.retain(|x| *x != handle);
        }
        if let Some(list) = inner.by_extension.get_mut(&hook.extension_id) {
            list.retain(|x| *x != handle);
        }
        Ok(())
    }
}
```

### 7.2 lifetime annotation 决定

- `BoxedHookFn` = `Box<dyn Fn(...) -> ... + Send + Sync + 'static>`：**`'static` 不可少**，因为闭包要在 host 内部活到注销
- extension 端如果想在 fn 里读自己的 state，做法是 `Arc::clone(&self.state)` 进 closure capture，**不能**裸借用
- `ExtensionScope` 持 `Weak<dyn HookHost>`（不是 `Arc`）—— 避免 host 与 extension 互持 Arc 形成循环

### 7.3 Pi `invalidate()` 反例对照（红线 11 邻居教训）

| Pi 行为（`runner.ts:466-478` + `loader.ts:154-167`）| zhive 改进 |
|---|---|
| 只设 `staleMessage` flag | host 端**主动** drain `by_extension[ext_id]` 全表 |
| 旧 handler 仍在 `extension.handlers` Map | 主动从 `table` / `by_event` / `by_extension` 三个 index 同时移除 |
| dispatch 时 throw + try/catch 吃错误 → log noise | dispatch 时 map 已无僵尸条目，零 throw |
| 没有 Drop 兜底 | `ExtensionScope::drop` 自动 unregister 残留 handles |

---

## 8. 14 个 event 对照表：触发 / 消费 / 是否允许 mutate

> 触发方 = engine 内部哪个模块发出（B1 决策）；消费方 = 谁监听（builtin / extension）；mutate = `MutateInput` 决策是否允许返回。

| # | Event | 触发方 | 主要消费方 | mutate 允许 | 备注 |
|---|---|---|---|---|---|
| 1 | `PreToolUse` | B1 agent loop（LLM 决定 tool call 时，执行前）| builtin permission-prompter、extension（user / project / mcp）| ✅ `tool_input` only | 红线 11；schema 重验证必走 |
| 2 | `PostToolUse` | B1 agent loop（tool execute 成功后）| extension audit-log、tracing | ❌ | mutate result 没意义（已用） |
| 3 | `PostToolUseFailure` | B1 agent loop（tool execute Err 后）| extension error-reporter、retry policy | ❌ | 失败原因写 transcript |
| 4 | `UserPromptSubmit` | B1 dispatcher（client `session/prompt` RPC 入口；A1 §2.3）| extension prompt-redactor / safety-filter | ⚠️ Phase 1 不允许 | Pi 允许 mutate prompt；zhive Phase 1 拒，理由：与 PreToolUse mutate 路径分立，避免心智模型膨胀。Phase 2 再开 |
| 5 | `SessionStart` | B1 `Engine::spawn` / thread restore（reason: startup / resume / clear / compact / fork）| builtin session-init、extension warmup | ❌ | |
| 6 | `SessionEnd` | B1 `Engine::shutdown` / thread close | extension cleanup | ❌ | |
| 7 | `SubagentStart` | B1 `spawn_subagent`（A1 / D-008 subagent inheritance）| builtin subagent-tracer | ❌ | base 内 `agent_id`/`agent_type`/`parent_tool_use_id` 必填 |
| 8 | `SubagentStop` | subagent agent loop 结束时 | builtin tracer、extension audit | ❌ | `stop_hook_active` 防递归 |
| 9 | `PreCompact` | B1 `compact()`（A3 PreCompact phase 进入时；token 阈值或手动）| builtin archive、extension custom-instructions | ⚠️ 仅 `custom_instructions` 字段 | 不允许动 `entries_count`/`trigger`；A4 此 event 设计 mutate 范围有限 |
| 10 | `PermissionRequest` | B1 permission reducer（A3）发起反向 RPC 前 | builtin permission-prompter、extension auto-approver | ⚠️ Phase 1 不允许 | mutate 推到 B6 reducer 落地 |
| 11 | `Stop` | B1 agent loop 决定 turn 结束时（`Stop` 与 `SessionEnd` 不同——Stop 是 turn 边界）| extension turn-finalizer | ❌ | `stop_hook_active` 防递归（同 Claude Code） |
| 12 | `Notification` | B1 / B4 任意 RPC notification 出口 | extension slack-forward 等外部桥接 | ❌ | category enum 6 值 |
| 13 | `Setup` | B1 `Engine::spawn` startup-once（trigger=init）/ 维护期（trigger=maintenance）| builtin bootstrap | ❌ | trigger enum 2 值 |
| 14 | `ToolApprovalChange` | A3 PermissionReducer（用户 toggle 或 hook 决策变更后）| builtin approval-tracker | ❌ | `origin` 区分 user / hook / scope-change |
| — | `PhaseTransition`（B1 §6.7 提议）| B1 phase_tx watch::Sender 切态时 | builtin metric / extension state-sync | ❌ | ⚠️ 待 A4 / D-012 修订决定是否进 15 |
| — | `Unknown { name, payload }` | 反序列化兜底（A4 §5）| ❌ 不可订阅 | ❌ | 仅 dispatch 路径 log + 转发；hook 不允许 register `Unknown` |

> TODO(B5-6)：`UserPromptSubmit` 是否允许 mutate prompt 在 Phase 1 关闭（保持决定），Phase 2 视用例需求再开。打开时同样适用红线 11 的"重验证"思路——但 prompt 是自由文本无 schema，需另立"长度上限 / 黑名单过滤"轻量验证。

---

## 9. 关键问题逐条作答

### Q1 · Hook 注册时机：startup 一次性扫盘 vs 每次 turn 重扫？

**startup 一次性扫盘**。理由：A5 §4 manifest 扫盘是 IO 密集（user / project / local 三层目录递归 + TOML parse），每次 turn 重扫成本高（多 thread 并发时 IO 风暴）。补 **手动 `/reload` 命令**（A5 §7.2 决定 A）覆盖"我改了 manifest 想立即生效"的 UX，**不做 fs-watch 自动重载**（inotify 跨平台坑 + 避免动态 race）。

### Q2 · Hook 执行模型：进程内 trait + JSON vs spawn 子进程？怎么共存？

**双轨并存**：进程内（`HookExecutor::InProcess(Arc<dyn HookFn>)`）+ 子进程（`HookExecutor::Subprocess(Arc<SubprocessSpec>)`）。理由：builtin hook 是 Rust 函数，进程内 trait 调用零 IPC 开销；子进程轨服务 manifest 出现 `entrypoint = "cmd:./hook.sh"` 形态的外部 hook（走 stdin JSON / stdout JSON 协议，对齐 Claude Code 文档 command-type hook，timeout 默认 600s）。两轨由 `register_hook` / `register_subprocess_hook` 分别注册，dispatch 时统一调度。

### Q3 · 多个 hook 对同一 event 的执行顺序：注册顺序 / namespace / priority？

**三键 lex order：`(source_rank, manifest_priority, registration_order)`**。详 §3.3。要点：(a) source 维度 `Builtin < User < Project < Local < Mcp`（builtin 最先跑，承担早期拦截责任）；(b) 同 source 看 manifest `[[hooks]] priority = N`（小先跑）；(c) 同 priority 看 atomic registration order。**不**按 namespace 单独 axis——`extension_ref.id` 已隐含 namespace（A5 `extension:` / `prompt:` / `skill:` 前缀），无需另立维度。

### Q4 · Hook timeout / panic 隔离：一个挂了怎么不连累 turn？

`tokio::time::timeout(hook.timeout, fut)` 包外层；`FutureExt::catch_unwind` 拦 panic 转 `HookFnError::Panic`；失败时 `tracing::warn!` 记录 + **跳过此 hook，继续下一个**（错误隔离，对齐 Pi `runner.ts:698-706` 但 Rust 化避免裸 unwind）。timeout 默认值按 manifest `[[hooks]] timeout` 字段读取，缺省 30s（对齐 Claude Code UserPromptSubmit 默认；其余 event 也用 30s——更激进的 600s 用于 subprocess hook，见 `DEFAULT_SUBPROCESS_TIMEOUT`）。turn-level cancel token 是**唯一**会打断 dispatch 的信号（select! 优先 race）。

### Q5 · 与 permission reducer（B6）的协作点

**B6 reducer 是 PreToolUse hook 链的**消费方**：**
1. B1 agent loop 调用 `hook_host.dispatch(HookEvent::PreToolUse(...))` 得到 `DispatchOutcome`
2. 若 `Blocked { reason, by }` → reducer 直接 fold 成 `PermissionDecision::Deny { reason }`
3. 若 `Aborted` → reducer fold 成 `PermissionDecision::AbortTurn`
4. 若 `MutatedInput { final_input }` → reducer 把 final_input 喂给下一阶段（permission scope 检查）
5. 若 `Continue` → reducer 走常规 scope 匹配路径
6. reducer 决定后单独发 `HookEvent::PermissionRequest`（hook chain 走第二轮 dispatch），允许 extension 二次干预 —— 这是"双层 hook"模式（PreToolUse = 行为前；PermissionRequest = 权限决策中）

折叠职责在 **B6 reducer**（不在 B5 host），host 只负责按 sort_key 串行调用并返 `DispatchOutcome`。

### Q6 · 红线 10 落地：`register_hook` API 怎么 enforce `registered_by`？

**位置参数 + 类型必填 + ExtensionRef constructor 不暴露给 extension**。详 §4。三道防护：
1. 编译期：`register_hook(scope, kind, **extension_ref: ExtensionRef**, prio, timeout, fn)` 第三参数无默认值，extension 必须传
2. 类型期：`ExtensionRef` 不是 Option / Option<Default>，不能传 `None`
3. 信任期：`ExtensionRef::user / project / local / mcp` 构造器**仅 host crate `pub(crate)`**；extension 拿不到。manifest 解析后由 host 端唯一注入 ext_ref，extension 端 register API 内部已经预先绑定身份（A5 loader 给每个 extension 发一个**已绑定 ext_ref 的 RegistrationProxy**，extension 只能调 proxy.register 不调 host.register_hook）

### Q7 · 红线 11 落地：`tool_call` mutate 后失败怎么处置？

**abort turn，不回滚到 mutate 前**。详 §6.2。理由：
- 一致性：与"hook 返回 AbortTurn"语义统一
- 可观测性：失败原因写 transcript（`Item::HookValidationError`），用户可见
- 简化：回滚 mutate 前 input 需要追踪 chain 中每一步副作用（哪些 hook 已经基于 invalid input 做过别的事？），rollback 复杂度高

abort 路径走 D-014 tracing + 写一条 `Item::HookValidationError { tool_name, by_extension, validation_errors }` 进 transcript，engine phase → Idle，client 收到 `turn/completed` notification with status=`AbortedByHook`。

### Q8 · Hook 失败时 pending queue 回滚的数据结构？

**`VecDeque<UserInput>` + `drain_and_try` 包装方法**。详 §5。语义 1:1 对齐 Pi `agent-harness.ts:391-401`：`drain(0..n).collect()` 拿快照 → 调用 `f` 处理 → Err 时 `push_front` 反向插回。VecDeque 选择理由：front 操作 O(1) amortized；Vec `splice(0..0, ..)` 是 O(N) move。**不**用 channel —— channel 一旦 recv 出来无法 push 回去（除非走中间缓存，比 VecDeque 复杂）。

### Q9 · Listener 生命周期（与 A5 scope token 衔接）

A5 §7.2 已定 client 侧（extension）持 `ExtensionScope` + `HookHandle`；本 deliverable §7 给出 host 侧三索引（`table` / `by_event` / `by_extension`）+ `unregister_scope` 实现。**双重保险**：
- 主路径（reload）：A5 §7.2 决定 D step 2 显式调 `host.unregister_scope(ext_id)`，host 端三索引同步剔除
- 兜底（异常路径如 panic 中途）：`ExtensionScope::drop` impl 通过 `Weak<dyn HookHost>` 升级后调 `unregister_one(handle)` 批量清理

**与 Pi 反例对比**：Pi `invalidate()` 只设 stale flag（`runner.ts:466-473`），handlers 仍残留在 `extension.handlers` Map，dispatch 时遍历 throw + try/catch 吃错误（`runner.ts:700-706`），导致 log noise + 性能浪费。zhive 主动 drain，dispatch 时 map 已无僵尸条目，零 noise。

---

## 10. 未决项

> TODO(B5-1)：`BoxedHookFn` 是否需支持 `&mut state` capture？Phase 1 builtin hook 多为无状态，先用 `Fn`；若出现需要可变状态的内置 hook，改 `Arc<Mutex<State>>` 闭包捕获，不动签名。

> TODO(B5-2)：A5 用 `schemars` 出 schema，校验侧用 workspace dep `jsonschema = "0.46"`（已落地）。

> TODO(B5-3)：`catch_unwind` 对 async closure 的支持需要 `futures::FutureExt::catch_unwind`（`futures` crate 是否在 workspace？）或手写 `AssertUnwindSafe` wrap——决定推到实装期。

> TODO(B5-4)：A4 `HookEventBase.registered_by` 字段在 dispatch 时由 host 端 stamp，需要 A4 deliverable 微调 mut 访问方式（要么字段 `pub`，要么 host 端 setter `with_registered_by(&mut self, ref)`）。

> TODO(B5-5)：若 Phase 2 出现需并行 hook 的 event（如纯 read-only `Notification`），考虑加 `[[hooks]] mode = "parallel"` manifest 字段，host 端按 mode 分桶执行。

> TODO(B5-6)：`UserPromptSubmit` 是否允许 mutate prompt 在 Phase 1 关闭（已决定保持关闭）；Phase 2 视用例再开。打开时同样适用红线 11 的"重验证"——但 prompt 是自由文本无 schema，需另立"长度上限 / 黑名单过滤"轻量验证。

> TODO(B5-7)：与 Claude Code 文档 "all matching hooks run in parallel" 的串行 vs 并行分歧需在 `decision-diffs.md` 记一条来源，避免 D-012 修订时被误以为漏看 Claude Code 文档。

> TODO(B5-8)：B1 提议的 `PhaseTransition` hook（B1 §6.7）是否进 D-012 第 15 个 event？本 deliverable §8 对照表标 ⚠️ 待 A4 / D-012 修订时决定。
