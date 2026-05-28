---
task: A4 — Hook event schema（14 + 5 reserved）
plan: phase1-core-native-research
date: 2026-05-28
status: draft
owner: A4 subagent
depends:
  - D-012（Hooks JSON schema 至少 14 事件 + `#[non_exhaustive]`）
  - 红线 10（hook base 必带 `registered_by: ExtensionRef`）
  - 红线 11（tool_call mutate input 后必须重验证）
consumes_by:
  - B5 hook host
---

# A4 · Hook event schema deliverable

> 决策冲突警告：D-012 关于 14 个事件的清单与 Claude Code 当前文档（2026-05）有 1 处命名漂移：D-012 写的 `ToolApprovalChange` 在 Claude Code 文档 19 事件表中**没有对应条目**，最接近的是 `PermissionRequest`（已在 D-012 14 清单中独立）。本 deliverable 暂保留 `ToolApprovalChange` 作为 zhive 自有事件（用于覆盖 user 手动 toggle permission_mode 的场景），但建议 D-012 修订时核对源出处。

> 决策冲突警告：D-012 把 `Setup` 列入 14 必含，但 Claude Code 文档将 `Setup` 标为 TypeScript-only。zhive 走 JSON-RPC + non-exhaustive enum，可以无害保留；若 Phase 1 不实装 `Setup` 触发逻辑，建议改为 reserved。

> 决策修订建议：D-012 reserved 5 个里 `WorktreeCreate / WorktreeRemove` 与 zhive Phase 1 不相关（zhive 不做 git 集成），建议下沉 Phase 3；同时 reserved 应**新增** `PreCompact` 的对偶 `PostCompact` 与 Pi 的 `SessionTree`/`BranchSummary`（见 § 6 Pi 差距）。

---

## 0. 摘要

- **14 个 case 的 `HookEvent` enum**（D-012 锁定）+ `#[non_exhaustive]` + 一个显式 `Unknown { name, payload }` case 处理未来 SDK 升级。
- **统一 `HookEventBase` wrapper**（不让各 event 自带 base 字段）—— wire 形如 `{ ...base..., hook_event_name: "...", ...event_specific... }`，反序列化用 `#[serde(flatten)]` 复用。
- **`registered_by: ExtensionRef` 进 base**，wire 字段名 `registered_by`，结构 `{ id, version, source: "user" | "project" | "local" | "builtin" }`，对齐 Pi `${PI}/packages/coding-agent/src/core/extensions/types.ts:551-557` 的 source 概念。
- **Subagent 上下文**（`agent_id / agent_type / parent_tool_use_id`）放 `HookEventBase` 而非各 event，**Option 化**（top-level agent 时为 None）。对齐 Claude Code TypeScript SDK "fields on the base hook input"。
- **不分 harness-level / extension-level** 两层 category。理由：zhive 的 JSON-RPC 反向请求模型不区分 harness 与 extension 调用者；权限/源追溯靠 `registered_by` 字段即可。Pi 分层是 TS in-process callback 注册 API 的副产物（harness 自己监听 vs extension 注册），不构成 wire schema 需求。
- **Pi 24 + 17 = 41 个 event** 与 zhive 14 的对照：14/41 ✅ 直接覆盖，5/41 通过 reserved 占位，13/41 ⚠️ 暂缺（compaction / branch_summary / leaf / model_select / tree 等），9/41 ❌ 拒收（in-process render / TypeBox schema / TS 专属机制等）。
- **`Unknown { name, payload }` 是反序列化未知 case 的降级出口**，不是删 `#[non_exhaustive]`。

---

## 1. 参考点清单

### 1.1 zhive 内部
| 路径 | 行号 | 用途 |
|---|---|---|
| `research/99-decisions/README.md` | L317-338 | D-012 决策原文（14 + 5 reserved 清单 + base 字段） |
| `research/99-decisions/README.md` | L435-437 | 红线 10（registered_by）、红线 11（mutate 后重验证） |
| `plans/phase1-core-native-research/phase1-core-native-research.md` | L187-214 | A4 任务定义本身 |
| `plans/phase1-core-native-research/phase1-core-native-research.md` | L388-418 | B5 hook host 任务（A4 schema 消费方） |

### 1.2 Pi
| 路径 | 行号 | 用途 |
|---|---|---|
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L494-499 | `ResourcesDiscoverEvent`（extension 启动期资源贡献，Pi 专属） |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L511-598 | `SessionEvent` 8 子事件（session_start / before_switch / before_fork / before_compact / compact / shutdown / before_tree / tree） |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L604-705 | Agent / Turn / Message / ToolExecution 12 个 event |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L711-765 | Model / ThinkingLevel / UserBash / Input 4 个 event |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L771-829 | `ToolCallEvent`（7 个内置工具 + Custom） |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L816-820 | **红线 11 反例**：`tool_call` mutate 无 re-validate |
| `${PI}/packages/coding-agent/src/core/extensions/types.ts` | L950-972 | `ExtensionEvent` 22-case union（extension-level） |
| `${PI}/packages/agent/src/harness/types.ts` | L485 | `AgentHarnessPhase` 5 态（idle/turn/compaction/branch_summary/retry） |
| `${PI}/packages/agent/src/harness/types.ts` | L493-514 | `QueueUpdateEvent` / `SavePointEvent` / `AbortEvent` / `SettledEvent`（harness 自身） |
| `${PI}/packages/agent/src/harness/types.ts` | L516-616 | harness own event 主体（17 case） |
| `${PI}/packages/agent/src/harness/types.ts` | L618-639 | `AgentHarnessOwnEvent` 17-case union 声明 |

### 1.3 Claude Code 文档
| URL | 用途 |
|---|---|
| https://code.claude.com/docs/en/agent-sdk/hooks | 19 事件命名权威 + base 字段（`session_id / cwd / hook_event_name`）+ subagent 字段（`agent_id / agent_type`）+ `permission_mode` |
| https://code.claude.com/docs/en/hooks | 各事件 JSON 输入 shape（PreToolUse / SessionStart / Setup / UserPromptSubmit / InstructionsLoaded / UserPromptExpansion 完整；其余 truncated） |

---

## 2. 关键问题逐条作答

### Q1 · `#[non_exhaustive]` + 反序列化未知 case 优雅降级，真的可行？

**答**：可行，但 `#[non_exhaustive]` 本身**不解决反序列化**，它只解决 **Rust 编译时**下游 match 的 forward-compat（强制 `_ => ...`）。反序列化降级要靠 **serde untagged fallback** 或 **catch-all variant**。

zhive 选**显式 `Unknown { name, payload }` catch-all variant**（不是 untagged + Option），理由：
- untagged 会让所有 Variant 反序列化都"先尝试再回落"，性能 O(n) 还可能误匹配。
- 显式 catch-all + `#[serde(other)]` 在 tagged enum 上原生支持，O(1)。
- `payload: serde_json::Value` 保留原始 JSON，hook host 可日志记录或转发但不解码。

最小 prototype 思路（**不进 crates/**）：

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "hook_event_name")]  // wire 内嵌 discriminator
#[non_exhaustive]                   // 强制下游 match 加 _
pub enum HookEvent {
    PreToolUse(PreToolUse),
    PostToolUse(PostToolUse),
    PostToolUseFailure(PostToolUseFailure),
    UserPromptSubmit(UserPromptSubmit),
    SessionStart(SessionStart),
    SessionEnd(SessionEnd),
    SubagentStart(SubagentStart),
    SubagentStop(SubagentStop),
    PreCompact(PreCompact),
    PermissionRequest(PermissionRequest),
    Stop(Stop),
    Notification(Notification),
    Setup(Setup),
    ToolApprovalChange(ToolApprovalChange),

    /// 反序列化降级出口：未来 SDK 加新 event，老 zhive 仍能消费。
    #[serde(other, rename = "_unknown")]
    Unknown,  // 注意：serde 的 #[serde(other)] 不允许带字段
}
```

> ⚠️ serde 已知限制：`#[serde(other)]` 在 internally tagged enum 上**只允许 unit variant**（不能带 payload）。要保留 raw payload，方案二是手写 `Deserialize` impl，把"未匹配 tag"的整个 `serde_json::Value` 装到 `Unknown { name: String, payload: Value }`。本 deliverable 推荐方案二，理由：unit variant 把原始数据丢了，没法转发也没法日志。

方案二骨架（仍然只在 deliverable 内示意）：

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookEvent {
    PreToolUse(PreToolUse),
    // ...（14 个 case 同上）
    Unknown { name: String, payload: serde_json::Value },
}

// 手写 Deserialize：先反序列化成 serde_json::Value，按 hook_event_name 分发；
// 落 Unknown 时保留原 payload；其它 case 走 serde_json::from_value::<T>(v)。
impl<'de> Deserialize<'de> for HookEvent { /* TODO(开放项 A4-Q1): 在 B5 实装时落地 */ }
```

> TODO(开放项 A4-Q1)：B5 实装 hook host 时落 `HookEvent` 的手写 `Deserialize`，并补一个 fuzz / property test 覆盖"未知 tag 必走 Unknown"。

### Q2 · base 字段放 wrapper 还是各 event 自带？

**答**：**放 wrapper（`HookEventBase`）**，所有 event payload 用 `#[serde(flatten)]` 嵌套。理由：

- Claude Code 文档明文："All hook inputs share `session_id`, `cwd`, and `hook_event_name`"——这是 wire 协议层面的共性，不是每 case 重复字段。
- Pi `AgentHarnessOwnEvent` 17 个 case 也没在每个 case 重复 base 字段，session 是 harness instance 持有的隐式上下文。zhive 把它显式化到 wire 上，但**仍然提取公共部分**避免 schemars 生成 14 份重复 schema。
- B5 host 端接收消息后第一步就是分发到 hook callback，base 字段是 dispatch 必读的（按 session_id 找 thread 等），独立 struct 更清晰。

线缆 wire 长这样（PreToolUse 例）：

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:filesystem", "version": "1.0", "source": "builtin" },
  "agent_id": null,
  "agent_type": null,
  "parent_tool_use_id": null,
  "tool_name": "bash",
  "tool_input": { "command": "ls" },
  "tool_use_id": "tool_xyz"
}
```

Rust 表征用 `#[serde(flatten)]`：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreToolUseInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_use_id: String,
}
```

### Q3 · Subagent 上下文字段放哪里？

**答**：**放 `HookEventBase`，Option 化**（top-level agent 时三个字段为 `None`）。理由：

- Claude Code TS SDK 明确"`agent_id` 和 `agent_type` 在 base hook input"。Python SDK 限 3 个 event 是 SDK 限制不是协议限制——zhive 走 JSON-RPC 走宽口径。
- subagent 上下文与 hook 分发逻辑解耦：B5 host 不需要按 event type 决定该不该读这三字段。
- Pi 没显式 subagent 字段是因为 Pi 没有 fresh-window subagent（Pi 的 "branch_summary" 是 session-tree 内分支不是新 agent）；这点是 zhive 自有需求，Pi 不提供锚点。
- `parent_tool_use_id` 见 Claude Code docs "Track subagent activity"，是 `SubagentStop` 用于关联触发它的 parent tool call。在 base 上挂为 Option 既能给 SubagentStart/Stop 用，也能给 subagent 内部 PreToolUse 用。

### Q4 · `registered_by` wire 字段名 / 编码？

**答**：

- wire 字段名：`registered_by`（snake_case，对齐 hook 其它 base 字段如 `session_id`）
- 类型：

  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
  pub struct ExtensionRef {
      /// 全局唯一 id，命名空间用 `<scope>:<name>`，scope ∈ {builtin, user, project, local, mcp}
      pub id: String,
      /// semver 字符串；builtin 用 zhive crate 自身版本
      pub version: String,
      /// 与 D-013 `settingSources` 三层（user/project/local）对齐，外加 builtin / mcp
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
  ```

- 强制性：`HookEventBase.registered_by` 不是 Option。B5 host 在 hook 注册路径（`register_hook(extension: ExtensionRef, ...)`）强制要求传入；hostbuiltin hook 自己填 `Builtin`。
- JSON 编码示例：`"registered_by": { "id": "user:my-skill", "version": "0.2.1", "source": "user" }`

> 锚点：Pi `${PI}/packages/coding-agent/src/core/extensions/types.ts:551-557` 用 `source: "quit" | "reload" | ...`，那是 reason；真正的 extension provenance 在 manifest 层（A5 处理）。zhive 把 provenance 升到 wire 是红线 10 的硬要求。

### Q5 · 是否分 harness-level / extension-level 两层（加 `category` 字段）？

**答**：**不分**。理由：

- Pi 分层是 TS in-process 注册 API 的产物：harness 自身用 `EventEmitter` 监听 `AgentHarnessOwnEvent`，extension 通过 manifest 注册 `ExtensionEvent`。两套触发 / 消费链路在 TS 一个 process 内并存才需要 category 区分订阅源。
- zhive 走 JSON-RPC 反向请求 / hook host 单一调度路径，所有 hook callback（无论来自 builtin / user / mcp）通过统一 `register_hook(ExtensionRef, HookEventKind, Callback)` 注册，调用方都是 B5 host，**不存在两条消费链**。
- 调用源用 `registered_by` 已经能定位（红线 10），category 是冗余 axis。
- 砍 category 也避免 wire 字段腐烂——D-012 14 个 event 全是 "harness-level"（session/tool/permission/subagent/notification 都是 zhive 核心关切），加 category 后所有 14 个都填同一值，没意义。

**保留扩展位**：若 Phase 2 出现真正的"extension 自有 event"（如 D-013 `ResourcesDiscoverEvent`），用 `Unknown { name, payload }` 承载 + 在 manifest 层注册即可，wire schema 不需要先验加 category。

### Q6 · Pi 多出来的 compaction / branch_summary / leaf 类事件是否补进 14 reserved？

**答**：**部分补**。详见 § 6 对照表。要点：

| Pi event | zhive 当前状态 | 建议 |
|---|---|---|
| `session_before_compact` | ⚠️ D-012 有 `PreCompact` 但没显式 `branchEntries`/`signal` 字段 | ✅ 已含，A4 schema 补 fixture（见 § 4） |
| `session_compact` | ❌ 缺 | **建议补 reserved**：`PostCompact`（PreCompact 对偶） |
| `session_before_tree` / `session_tree` | ❌ 缺 | **不进 Phase 1**（zhive 暂不做 session tree fork UI；JSONL leaf 写盘是 B3 的事，不需 hook 通知 extension） |
| `branch_summary_*` | ❌ 缺 | **不进 Phase 1**（Pi 专属，依赖其 session-tree 模型） |
| `before_provider_request` / `after_provider_response` | ❌ 缺 | **建议下沉 reserved**：`PreProviderRequest` / `PostProviderResponse`（B10 provider 抽象成形后开） |
| `model_select` / `thinking_level_select` | ❌ 缺 | Phase 1 不开（zhive 还没决定 LLM provider switch UX） |
| `resources_discover` | ❌ 缺 | A5 manifest 层处理（D-013 待定项）；不是通用 hook |

reserved 由 5 改建议 6（用 `PostCompact` 替换 `WorktreeRemove`），但**这是 § 0 顶部的修订建议，不在本 deliverable 落地**——按红线 "不改 research/99-decisions" 提交到 `decision-diffs.md` 走用户回流。

---

## 3. `HookEvent` enum 14 case + `HookEventBase` Rust 草图

```rust
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

// ─────────────────────────────────────────────────────────────────────────────
// 共用基座
// ─────────────────────────────────────────────────────────────────────────────

/// 所有 hook event payload 共享的 base 字段。
///
/// wire 上通过 `#[serde(flatten)]` 内嵌到每个 event 的 JSON object，
/// 不构成独立 nested object。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub struct HookEventBase {
    /// Thread / session 标识。对齐 D-006 Thread.id。
    pub session_id: String,

    /// 触发 hook 时的工作目录（绝对路径）。
    pub cwd: String,

    /// 红线 10：hook 注册者来源。**必填**。
    pub registered_by: ExtensionRef,

    /// Subagent 标识。top-level agent 时为 None。
    /// 对齐 Claude Code TS SDK "base hook input" 字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// Subagent 类型（e.g. "explore" / "security-reviewer"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,

    /// 触发当前 subagent 的 parent tool_use id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,

    /// 触发时的 permission_mode 快照（用于审计 / 回放）。
    /// 与 A3 `PermissionScope` 解耦：scope 是当前生效策略，mode 是用户 UI 选择。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,

    /// Transcript JSONL 文件路径（对齐 B3 rollout 写盘位置）。
    /// 让 hook 可以 read-only 访问全 turn 历史而不走 RPC 回查。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
pub struct ExtensionRef {
    pub id: String,
    pub version: String,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
}

// ─────────────────────────────────────────────────────────────────────────────
// HookEvent 主 enum（14 case + Unknown）
// ─────────────────────────────────────────────────────────────────────────────

/// 14 个 Phase 1 hook event + Unknown 降级出口。
///
/// `#[non_exhaustive]` 强制下游 match 加 `_` 分支（红线 10 配合：未来加 case 不破 ABI）。
/// `Unknown { name, payload }` 是反序列化未知 `hook_event_name` 的兜底（手写 Deserialize 落地，见 § 2 Q1）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "hook_event_name")]
#[non_exhaustive]
pub enum HookEvent {
    PreToolUse(PreToolUseInput),
    PostToolUse(PostToolUseInput),
    PostToolUseFailure(PostToolUseFailureInput),
    UserPromptSubmit(UserPromptSubmitInput),
    SessionStart(SessionStartInput),
    SessionEnd(SessionEndInput),
    SubagentStart(SubagentStartInput),
    SubagentStop(SubagentStopInput),
    PreCompact(PreCompactInput),
    PermissionRequest(PermissionRequestInput),
    Stop(StopInput),
    Notification(NotificationInput),
    Setup(SetupInput),
    ToolApprovalChange(ToolApprovalChangeInput),

    // ⚠️ serde 限制：tagged enum 的 #[serde(other)] 必须是 unit variant。
    // 实际工程中用手写 Deserialize 把未知 tag 装入下面的 variant：
    #[serde(skip)]
    Unknown { name: String, payload: serde_json::Value },
}

// ─────────────────────────────────────────────────────────────────────────────
// 14 个 event payload struct
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreToolUseInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PostToolUseInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_response: serde_json::Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PostToolUseFailureInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_use_id: String,
    /// 失败原因（thiserror display 后的 String）
    pub error: String,
    /// 失败类型（用于上游 reducer 决定是否 retry）
    pub error_kind: ToolErrorKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    Timeout,
    Cancelled,
    InvalidInput,
    PermissionDenied,
    ExecutionError,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UserPromptSubmitInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    /// 用户原始输入文本（slash command 展开后）
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionStartInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    /// 启动原因，对齐 Claude Code `source` 字段
    pub source: SessionStartSource,
    /// LLM model 标识符（Phase 1 占位；B10 provider 落地后定）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
    Fork,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionEndInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub reason: SessionEndReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    Clear,
    Resume,
    Logout,
    PromptInputExit,
    BypassPermissionsDisabled,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentStartInput {
    #[serde(flatten)]
    pub base: HookEventBase,  // base 内 agent_id / agent_type / parent_tool_use_id 必填
    /// 父→子继承的 permission scope 快照（A3）
    pub inherited_scope: serde_json::Value,  // TODO(A3 deliverable 收敛后改 PermissionScope)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentStopInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    /// subagent 自己的 transcript（独立于父 transcript）
    pub agent_transcript_path: String,
    /// 是否被父 Stop hook 触发回收（防递归 Stop 用）
    #[serde(default)]
    pub stop_hook_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PreCompactInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub trigger: CompactTrigger,
    /// 即将压缩的 item 数量
    pub entries_count: u32,
    /// 用户自定义 compaction 指令（CLI 传入）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequestInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_use_id: String,
    /// 当前请求的权限 scope（来自 A3）
    pub requested_scope: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StopInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    /// 防递归 Stop 调用（Claude Code 同字段）
    #[serde(default)]
    pub stop_hook_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotificationInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub category: NotificationCategory,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCategory {
    PermissionPrompt,
    IdlePrompt,
    AuthSuccess,
    ElicitationDialog,
    ElicitationResponse,
    ElicitationComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetupInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub trigger: SetupTrigger,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SetupTrigger {
    Init,
    Maintenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolApprovalChangeInput {
    #[serde(flatten)]
    pub base: HookEventBase,
    pub tool_name: String,
    /// 旧策略
    pub previous: ToolApprovalState,
    /// 新策略
    pub current: ToolApprovalState,
    /// 触发源：用户手动 / hook 决策 / scope 变更
    pub origin: ToolApprovalOrigin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalState {
    Allow,
    AllowOnce,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalOrigin {
    UserDecision,
    HookDecision,
    ScopeChange,
}
```

> TODO(开放项 A4-S1)：`SubagentStartInput.inherited_scope` 与 `PermissionRequestInput.requested_scope` 当前是 `serde_json::Value`，A3 deliverable 收敛 `PermissionScope` 后改回 typed。
>
> TODO(开放项 A4-S2)：`SessionStartInput.model` 是 Phase 1 占位字段，B10 provider 决定 LLM provider trait 后再敲定。

---

## 4. Hook input JSON 示例 × 14

> 所有 fixture 共享一个虚拟 base，重点突出 event-specific 字段。`session_id` 都用 `"thread_abc"`，registered_by 都填一个 builtin 示例。

### 4.1 PreToolUse

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:filesystem-guard", "version": "0.1.0", "source": "builtin" },
  "permission_mode": "default",
  "transcript_path": "/home/user/.zhive/rollouts/thread_abc.jsonl",
  "tool_name": "bash",
  "tool_input": { "command": "rm -rf /" },
  "tool_use_id": "tooluse_01"
}
```

### 4.2 PostToolUse

```json
{
  "hook_event_name": "PostToolUse",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "user:audit-log", "version": "0.2.1", "source": "user" },
  "tool_name": "bash",
  "tool_input": { "command": "ls -la" },
  "tool_response": { "stdout": "total 0\n", "stderr": "", "exit_code": 0 },
  "tool_use_id": "tooluse_02"
}
```

### 4.3 PostToolUseFailure

```json
{
  "hook_event_name": "PostToolUseFailure",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:error-reporter", "version": "0.1.0", "source": "builtin" },
  "tool_name": "bash",
  "tool_input": { "command": "false" },
  "tool_use_id": "tooluse_03",
  "error": "command exited with code 1",
  "error_kind": "execution_error"
}
```

### 4.4 UserPromptSubmit

```json
{
  "hook_event_name": "UserPromptSubmit",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "project:prompt-redactor", "version": "0.0.3", "source": "project" },
  "permission_mode": "default",
  "prompt": "summarize the diff"
}
```

### 4.5 SessionStart

```json
{
  "hook_event_name": "SessionStart",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:session-init", "version": "0.1.0", "source": "builtin" },
  "source": "startup",
  "model": "anthropic/claude-opus-4-7"
}
```

### 4.6 SessionEnd

```json
{
  "hook_event_name": "SessionEnd",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "user:cleanup", "version": "1.0.0", "source": "user" },
  "reason": "prompt_input_exit"
}
```

### 4.7 SubagentStart

```json
{
  "hook_event_name": "SubagentStart",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:subagent-tracer", "version": "0.1.0", "source": "builtin" },
  "agent_id": "sub_01",
  "agent_type": "explore",
  "parent_tool_use_id": "tooluse_04",
  "inherited_scope": { "fs_read": ["/home/user/proj/**"], "fs_write": [], "exec": [] }
}
```

### 4.8 SubagentStop

```json
{
  "hook_event_name": "SubagentStop",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:subagent-tracer", "version": "0.1.0", "source": "builtin" },
  "agent_id": "sub_01",
  "agent_type": "explore",
  "parent_tool_use_id": "tooluse_04",
  "agent_transcript_path": "/home/user/.zhive/rollouts/thread_abc.sub_01.jsonl",
  "stop_hook_active": false
}
```

### 4.9 PreCompact

```json
{
  "hook_event_name": "PreCompact",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "user:archive-before-compact", "version": "0.3.0", "source": "user" },
  "trigger": "auto",
  "entries_count": 142,
  "custom_instructions": "keep all file edits verbatim"
}
```

### 4.10 PermissionRequest

```json
{
  "hook_event_name": "PermissionRequest",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:permission-prompter", "version": "0.1.0", "source": "builtin" },
  "tool_name": "bash",
  "tool_input": { "command": "git push" },
  "tool_use_id": "tooluse_05",
  "requested_scope": { "exec": ["git push"] }
}
```

### 4.11 Stop

```json
{
  "hook_event_name": "Stop",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:turn-finalizer", "version": "0.1.0", "source": "builtin" },
  "stop_hook_active": false
}
```

### 4.12 Notification

```json
{
  "hook_event_name": "Notification",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "user:slack-forward", "version": "0.4.2", "source": "user" },
  "category": "permission_prompt",
  "title": "Approve bash command",
  "message": "Agent wants to run: git push"
}
```

### 4.13 Setup

```json
{
  "hook_event_name": "Setup",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:bootstrap", "version": "0.1.0", "source": "builtin" },
  "trigger": "init"
}
```

### 4.14 ToolApprovalChange

```json
{
  "hook_event_name": "ToolApprovalChange",
  "session_id": "thread_abc",
  "cwd": "/home/user/proj",
  "registered_by": { "id": "builtin:approval-tracker", "version": "0.1.0", "source": "builtin" },
  "tool_name": "bash",
  "previous": "ask",
  "current": "allow",
  "origin": "user_decision"
}
```

---

## 5. 反序列化未知 case 的策略

**选择：保留 raw JSON，落到 `HookEvent::Unknown { name, payload: serde_json::Value }`。**

理由对比：

| 方案 | 优点 | 缺点 | 决定 |
|---|---|---|---|
| A. 完全拒绝（serde 报错） | 最严格 | 一次 SDK 升级整个客户端起不来 | ❌ |
| B. `#[serde(other)]` unit variant | 无 panic、零代码 | 丢 payload，hook host 无法转发也无法日志 | ❌ |
| C. **`Unknown { name, payload }` + 手写 Deserialize** | 保留原文本，可日志可前向转发 | 多 ~30 行手写 Deserialize | ✅ |
| D. untagged + Option fallback | 不需要 manual impl | 性能 O(n) + 误匹配风险（PreToolUse 字段也能填进 PostToolUse） | ❌ |

落地点：
- 反序列化路径：B5 `HookHost::dispatch(raw_msg)` 进 enum；hit Unknown 时打 `tracing::warn!` 但不 abort。
- 序列化路径：Unknown 重新 emit 时按原 `name` 写 `hook_event_name` + 原 payload 字段（用于反向 RPC 透传给 client）。
- 单元测试要求（B5 落实）：
  - "未来加 `PostCompact` event" → 老 zhive deserialize 成 `Unknown { name: "PostCompact", payload: { ... } }`。
  - Unknown 不允许 mutate（红线 11 不适用，因为 host 也不知道该重验证什么）。

> TODO(开放项 A4-S3)：B5 实装 `HookEvent` 的 `Deserialize` 时，约定 Unknown event 在 hook dispatch 路径"只发不收"——extension 不能注册监听 `Unknown`，host 也不会触发它。

---

## 6. 与 Pi 24 + 17 事件的逐项对照表

> Pi `ExtensionEvent` 22 case（types.ts:950-972）+ `AgentHarnessOwnEvent` 17 case（types.ts:618-639）= 39 case，其中部分重名（如 `tool_call` / `tool_result`）。下表合并去重后 ~37 case，每条标 zhive 处置。

### 6.1 Session / Resources 类

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `session_start` | ✅ `SessionStart` | 1:1 对应；Pi `reason` 4 值 → zhive `source` 5 值（补 Fork） |
| `session_before_switch` | ❌ 拒 | zhive 无 multi-session-file 模型（D-006 Thread 是顶层），不支持 switch |
| `session_before_fork` | ⚠️ 缺 | reserved 候选；JSONL Leaf fork（B3）发生时可发，但 Phase 1 不开 hook |
| `session_before_compact` | ✅ `PreCompact` | Pi 有 `branchEntries` / `signal`，zhive 简化为 `entries_count` + `custom_instructions`（signal 走 RPC cancel 通道） |
| `session_compact` | ⚠️ 缺 | **建议补 reserved `PostCompact`**（见 § 0 修订建议） |
| `session_shutdown` | ✅ `SessionEnd` | 名字不同；reason 集合扩展（Pi 4 → zhive 6） |
| `session_before_tree` | ❌ 拒 | Pi 专属 session-tree fork UI，zhive Phase 1 不做 |
| `session_tree` | ❌ 拒 | 同上 |
| `resources_discover` | ❌ 拒（Phase 1） | 走 A5 manifest 而不是 hook |

### 6.2 Agent / Turn / Message 类

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `before_agent_start` | ⚠️ 缺 | zhive 把"修改 system prompt"放 A2 capabilities 协商而不是 hook |
| `agent_start` | ⚠️ 缺 | Phase 1 不需要——Engine 内部信号，不必通过 hook 协议外发 |
| `agent_end` | ⚠️ 缺 | 同 `Stop`？需 B1 engine loop 定 turn vs agent loop 边界 |
| `turn_start` | ⚠️ 缺 | Phase 1 zhive 通过 RPC notification `TurnStart` 发（D-010），不上 hook |
| `turn_end` | ⚠️ 缺 | 同上 |
| `message_start` | ⚠️ 缺 | 流式消息开始；zhive 用 RPC notification 发 |
| `message_update` | ⚠️ 缺 | 流式 chunk；zhive 用 RPC notification 发 |
| `message_end` | ⚠️ 缺 | 同上 |
| `context` | ❌ 拒 | 让 extension 改 messages，太宽口；zhive 用 PreToolUse + permission reducer 替代 |
| `before_provider_request` | ⚠️ 缺 | 下沉 reserved（B10 provider 落地后）|
| `before_provider_payload` | ❌ 拒 | 暴露 LLM provider 原 payload 违反抽象层 |
| `after_provider_response` | ⚠️ 缺 | 下沉 reserved |

### 6.3 Tool 类

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `tool_call` | ✅ `PreToolUse` | Pi 文档对应；mutate 后**zhive 红线 11 强制重验证** |
| `tool_result` | ✅ `PostToolUse` | Pi 有 isError 区分；zhive 拆 `PostToolUse` + `PostToolUseFailure` 两 event |
| `tool_execution_start` | ❌ 拒 | Phase 1 zhive 不暴露执行内细粒度（用 transcript 取 detail） |
| `tool_execution_update` | ❌ 拒 | 同上（流式更新走 RPC notification） |
| `tool_execution_end` | ❌ 拒 | 同上 |

### 6.4 Permission / Subagent 类

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| （Pi 无对应） | ✅ `PermissionRequest` | zhive 独有：A3 reducer + permission scope 模型需要 |
| （Pi 无对应） | ✅ `SubagentStart` | zhive 独有：D-008 subagent inheritance 需要 |
| （Pi 无对应） | ✅ `SubagentStop` | 同上 |
| （Pi 无对应） | ✅ `ToolApprovalChange` | 用户手动 toggle approval 时通知 extension |
| （Pi 无对应） | ✅ `Notification` | 对齐 Claude Code，泛通知通道 |

### 6.5 User Input 类

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `input` | ✅ `UserPromptSubmit` | Pi `source: interactive/rpc/extension` 信息通过 A1 Item 区分，不进 hook payload |
| `user_bash` | ❌ 拒 | Pi 专属 `!` / `!!` prefix UX，zhive 没这个语法 |

### 6.6 Model / Misc

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `model_select` | ⚠️ 缺 | Phase 1 不开（B10 provider 抽象未定） |
| `thinking_level_select` | ⚠️ 缺 | 同上 |
| `resources_update` | ❌ 拒 | A5 manifest 内部事件 |

### 6.7 Harness own event（Pi `AgentHarnessOwnEvent`）

| Pi event | zhive 覆盖 | 备注 |
|---|---|---|
| `queue_update` | ❌ 拒 | A3 三队列状态走 RPC notification，不上 hook |
| `save_point` | ❌ 拒 | B3 persistence 内部，hook 不可见 |
| `abort` | ❌ 拒 | A3 + B7 取消通过 `$/cancelRequest` 走 RPC，不进 hook |
| `settled` | ❌ 拒 | A3 `nextTurnCount` 走 notification |
| 其余 13 个（context/turn/message 等） | 见 6.2/6.3 | 同上 |

### 6.8 汇总

| 分类 | 数量 |
|---|---|
| Pi event 总（去重后） | 37 |
| zhive ✅ 覆盖 | 14（D-012 全集） |
| zhive ⚠️ Phase 2/3 reserved | 13（agent / turn / message / provider / fork / compact post / model） |
| zhive ❌ 拒 | 10（render 类 / harness 内部 / session-tree / context-mutate / user_bash 等） |

zhive **独有 5 个**（Pi 无对应）：`PermissionRequest / SubagentStart / SubagentStop / ToolApprovalChange / Notification`——其中 4 个对齐 Claude Code（不是 Pi）。

---

## 7. 关键设计选择回顾

| 选择 | 决定 | 拒因 |
|---|---|---|
| `non_exhaustive` 上不上 | **上** | 不上则未来加 case 破 ABI |
| 反序列化兜底 | **`Unknown { name, payload }` 手写 Deserialize** | `#[serde(other)]` unit variant 丢 payload；untagged O(n)；拒绝模式 fragile |
| base 字段放哪 | **wrapper `HookEventBase` + `#[serde(flatten)]`** | 各 event 自带导致 schemars 14 份重复；分发逻辑也要 14 次提取 |
| subagent 字段放哪 | **base 的 Option 字段** | 放各 event 导致 dispatch 要 14 次条件读 |
| `registered_by` 强制性 | **非 Option，必填** | 红线 10 字面要求；builtin hook 自己填 Builtin |
| 是否分 category（harness/extension） | **不分** | Pi 分层是 in-process 注册 API 副产物，zhive JSON-RPC 单一调度链不需要 |
| Pi compaction / branch_summary 补不补 | **PostCompact 进 reserved；tree / branch_summary 不进** | tree fork UX 是 Pi 专属，zhive Phase 1 无需求 |
| `transcript_path` 字段 | **base Option 字段** | 对齐 Claude Code；让 hook read-only 拿全 turn 历史不必走 RPC 回查 |
| `permission_mode` 字段 | **base Option 字段（不是 enum 中各 event 自带）** | Claude Code 部分 event 没这字段（如 SessionStart），用 Option 优雅 |

---

## 8. 未决项

> TODO(开放项 A4-Q1)：B5 实装 hook host 时落 `HookEvent` 的手写 `Deserialize`，并补 fuzz / property test 覆盖"未知 tag 必走 Unknown"。

> TODO(开放项 A4-S1)：`SubagentStartInput.inherited_scope` 与 `PermissionRequestInput.requested_scope` 待 A3 `PermissionScope` 收敛后改 typed。

> TODO(开放项 A4-S2)：`SessionStartInput.model` 字段是 B10 provider 占位，B10 决定 provider trait 后敲定 model identifier 编码（`provider:model` vs URI-style）。

> TODO(开放项 A4-S3)：`HookEvent::Unknown` 单元测试在 B5 落实，约定 extension 不能注册监听 `Unknown`。

> TODO(开放项 A4-D1)：D-012 reserved 5 个建议改 6 个：用 `PostCompact` 替换 `WorktreeRemove`，新增 `PreProviderRequest` / `PostProviderResponse` 占位；待 `decision-diffs.md` 集中提交。

> TODO(开放项 A4-D2)：`ToolApprovalChange` 在 Claude Code 19 事件中无对应；是否真的是 zhive 必需 14 之一，或换 Claude Code 的 `MessageDisplay`，等 D-012 修订时决定。

> TODO(开放项 A4-D3)：`Setup` 在 Claude Code 是 TS-only；zhive 是否实装 Setup 触发逻辑（init / maintenance），由 B1 engine loop deliverable 落地时定。

> TODO(开放项 A4-Q5)：未来若发现 builtin hook 与 extension hook 在调度顺序 / 错误隔离上有刚性差异（B5 实测），重新评估是否加 `category` 字段。当前判断不加。
