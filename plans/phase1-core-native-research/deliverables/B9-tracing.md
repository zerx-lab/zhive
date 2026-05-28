---
task: B9 — tracing spans + 字段命名（OTel-aligned）
title: tracing spans 覆盖矩阵 / 字段命名 / 日志级别 / subscriber 初始化
date: 2026-05-28
status: draft
depends_on:
  - D-014（tracing 进核心；OTel exporter feature gate）
  - R-6（Phase 1 不装 tracing-opentelemetry，但字段名按 OTel semconv 预先对齐）
  - B1 deliverable（EnginePhase 6 态 / broadcast event 1024 / watch phase / mpsc submission 512）
  - A1 deliverable（Turn lifecycle + `turn/started` / `turn/completed` notification）
  - A4 deliverable（Hook 14 事件 + `HookEventBase`）
references:
  - https://docs.rs/tracing/latest/tracing/                                     (tracing 6 macro: trace/debug/info/warn/error + span/instrument)
  - https://docs.rs/tracing-subscriber/latest/tracing_subscriber/               (fmt + env-filter + Registry layered subscriber)
  - https://docs.rs/tracing-opentelemetry/latest/tracing_opentelemetry/         (Phase 1 不装，仅命名对齐参考)
  - https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/            (GenAI spans: gen_ai.operation.name / gen_ai.request.model / gen_ai.usage.* / gen_ai.tool.name / gen_ai.conversation.id)
  - https://opentelemetry.io/docs/specs/semconv/rpc/json-rpc/                   (JSON-RPC: rpc.system.name=jsonrpc / rpc.method / jsonrpc.protocol.version / jsonrpc.request.id / rpc.response.status_code)
  - https://opentelemetry.io/docs/specs/semconv/registry/attributes/session/    (session.id —— 通用 conversation/thread id)
  - https://opentelemetry.io/docs/specs/semconv/attributes-registry/#error      (error.type 通用属性)
  - research/99-decisions/README.md                                              L364-376 (D-014 决策原文 + 强制覆盖 6 个 span 名)
  - plans/phase1-core-native-research/phase1-core-native-research.md             L494-515 (B9 任务定义) / L700 (R-6 风险)
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md             L72-179 (EnginePhase 6 态定义) / L614-712 (channel 拓扑)
  - plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md       L738-750 (TurnStartedNotification / TurnCompletedNotification wire)
  - plans/phase1-core-native-research/deliverables/A4-hook-event-schema.md      L82-122 (HookEvent 14 case enum) / L139-167 (HookEventBase + PreToolUse 示例)
---

> 设计衔接警告：D-014 原文仅要求"覆盖 6 个 span（Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint）"+"`tracing-opentelemetry` feature gate"+"`tracing-subscriber` 仅启 `fmt + env-filter`"。本 deliverable 在此之上补：(a) 6 个 span 的具体 instrument 位置与 B1 EnginePhase 的对齐，(b) span 字段命名规约对照 OTel semantic conventions 的逐项映射，(c) 日志级别 5 级使用守则。**不改 D-014**——本文是落地补充，所有"扩展"以 `> TODO(开放项 B9-N)` 形式回流至 §7 未决项。

---

## 1. 参考点清单

下面所有论断均回指此清单，逐条按"出处 + 锚点"定位。

### 1.1 zhive 内部锚点

| 主题 | 路径 | 行号 |
|---|---|---|
| D-014 决策原文：6 个 span 强制覆盖 + OTel feature gate + subscriber 仅 fmt+env-filter | `research/99-decisions/README.md` | 364-376 |
| R-6 风险：Phase 1 不装 tracing-opentelemetry，字段名按 OTel semconv 起避免后悔 | `plans/phase1-core-native-research/phase1-core-native-research.md` | 700 |
| B9 任务定义本身 | `plans/phase1-core-native-research/phase1-core-native-research.md` | 494-515 |
| EnginePhase 6 态（Idle / Turn / Compaction / BranchSummary / Retry / SubagentSpawn） | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 72-98 |
| EnginePhase 与 TurnStatus 正交矩阵 | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 183-196 |
| broadcast event 容量 1024 / watch phase / mpsc submission 512 选型 | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 714-731 |
| `TurnStartedNotification { thread_id, turn }` / `TurnCompletedNotification { thread_id, turn }` wire 形态 | `plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md` | 738-750 |
| Hook 14 case `HookEvent` enum + `#[non_exhaustive]` | `plans/phase1-core-native-research/deliverables/A4-hook-event-schema.md` | 82-122 |
| `HookEventBase { session_id, cwd, hook_event_name, registered_by, agent_id, agent_type, parent_tool_use_id }` | `plans/phase1-core-native-research/deliverables/A4-hook-event-schema.md` | 139-167 |
| B1 `PhaseTransition` 通用 hook（与 D-012 14 共存） | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | 788-800 |

### 1.2 OTel semantic conventions 锚点（外部权威）

| 主题 | URL section |
|---|---|
| GenAI Inference / Tool span 命名 + 属性（`gen_ai.operation.name` / `gen_ai.request.model` / `gen_ai.usage.input_tokens|output_tokens` / `gen_ai.tool.name` / `gen_ai.tool.call.id` / `gen_ai.conversation.id` / `error.type`） | https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/ |
| JSON-RPC 属性（`rpc.system.name=jsonrpc` / `rpc.method` / `jsonrpc.protocol.version` / `jsonrpc.request.id` / `rpc.response.status_code`） | https://opentelemetry.io/docs/specs/semconv/rpc/json-rpc/ |
| Session id 通用属性（`session.id` 用于 conversation/thread/session 关联） | https://opentelemetry.io/docs/specs/semconv/registry/attributes/session/ |
| `error.type` 通用错误分类（如 `timeout` / `_OTHER`） | https://opentelemetry.io/docs/specs/semconv/attributes-registry/ |
| tracing crate 6 个 macro：`trace! / debug! / info! / warn! / error! / span!`（以及 `#[instrument]`） | https://docs.rs/tracing/latest/tracing/ |
| `tracing-subscriber` Registry 分层（`fmt + env-filter + opentelemetry layer` 三选一可叠加） | https://docs.rs/tracing-subscriber/latest/tracing_subscriber/ |

> 锚点说明：本 deliverable 不要求阅读 OTel 全部 attribute registry，只锚定 zhive 直接用到的 GenAI / RPC / Session / Error 四域。其余（network / cloud / k8s 等）不进 Phase 1 命名规约。

---

## 2. 6 个必覆盖 span 矩阵（D-014 强制清单的落地）

### 2.1 span 总表

| # | span 名（zhive） | OTel 推荐命名 | instrument 位置（B1 锚点） | 关键字段（zhive 字段 / OTel 对照） | 父 span | EnginePhase 对应 |
|---|---|---|---|---|---|---|
| 1 | `zhive.turn` | `chat {gen_ai.request.model}`（GenAI Inference 命名规约） | `Engine::start_turn` 入口（B1 §4 `start_turn` 方法）；持续到 `turn/completed` notification 发出 | `thread_id` → `gen_ai.conversation.id` / `session.id`，`turn_id` → `gen_ai.conversation.id.subspan`（zhive 自有），`model` → `gen_ai.request.model`，`gen_ai.operation.name=chat`，`gen_ai.usage.input_tokens`，`gen_ai.usage.output_tokens`，`turn.status` → `error.type` 当 Failed / Interrupted | root | `EnginePhase::Turn`（含 in-turn `Retry` 子状态） |
| 2 | `zhive.hook` | `execute_tool {gen_ai.tool.name}` 不适用 —— hook 不是 tool；采用 zhive 自有命名 `hook.{hook_event_name}`（如 `hook.PreToolUse`） | B5 `HookHost::dispatch(event)` 单一入口（A4 §3 `HookEvent` enum） | `hook_event_name` → 内嵌在 span name，`session_id` → `session.id`，`registered_by.id` → `code.namespace`，`registered_by.source` → zhive 自有 `zhive.extension.source`，`agent_id` / `agent_type` / `parent_tool_use_id` → 都 Option 转 `gen_ai.agent.id` / `gen_ai.agent.name` / zhive 自有 `zhive.parent_tool_call_id` | `zhive.turn`（多数）/ `zhive.subagent`（subagent 内）/ root（SessionStart） | 任何 phase 都可能触发，但每个 hook event 类型与 phase 有强相关（PreToolUse ⊂ Turn / PreCompact ⊂ Compaction） |
| 3 | `zhive.subagent` | `chat {model}` 子 span，加 `gen_ai.agent.id` / `gen_ai.agent.name` | `Engine::spawn_subagent` 入口（B1 §4）；持续直到 child engine 上首个 turn 结束并 final message 回流父 | `parent_thread_id` → `zhive.parent.session.id` (zhive 自有)，`child_thread_id` → `session.id`，`agent_type` → `gen_ai.agent.name`，`spawn_reason` → zhive 自有 `zhive.subagent.spawn_reason` | `zhive.turn`（父 turn） | `EnginePhase::SubagentSpawn` 期间，结束后父回 `Turn` |
| 4 | `zhive.permission` | 无 OTel 对应 —— zhive 自有；span name `permission.{tool_name}` | A3 `PermissionReducer::evaluate` 入口；反向 RPC `permission/request` 发出到 `permission/response` 回收 | `tool_name` → `gen_ai.tool.name`，`tool_call_id` → `gen_ai.tool.call.id`，`decision` → zhive 自有 `zhive.permission.decision`（`allow / deny / once / always_allow`），`source` → zhive 自有 `zhive.permission.source`（user / policy / inherited） | `zhive.tool_call` 或 `zhive.hook`（PermissionRequest） | 通常 `Turn` 内；subagent 时在 `SubagentSpawn` 内 |
| 5 | `zhive.tool_call` | `execute_tool {gen_ai.tool.name}`（OTel GenAI 标准） | LLM provider stream 解析出 `tool_call` item 时打开 span；tool 返回 result 时关闭 | `tool_name` → `gen_ai.tool.name`，`tool_call_id` → `gen_ai.tool.call.id`，`tool_input` → 不上 wire（PII / 体积大；只在 debug! 级日志中），`tool_result.status` → `error.type` 当失败 | `zhive.turn` | `EnginePhase::Turn` 内 |
| 6 | `zhive.rollback_point` | 无 OTel 对应 —— zhive 自有；span name `rollback_point` | B2 state-memory rollback 触发点（fork / undo / branch_summary 时记录的"可回滚锚点"创建） | `thread_id` → `session.id`，`rollback_id` → zhive 自有 `zhive.rollback.id`，`reason` → zhive 自有 `zhive.rollback.reason`（user_fork / auto_branch_summary） | `zhive.turn` 或 root（用户主动 fork 在 Idle） | `Idle` / `BranchSummary` / `Turn` 内都可能 |

### 2.2 span 与 EnginePhase 6 态对应矩阵

| EnginePhase | 哪个 span active？ | 备注 |
|---|---|---|
| `Idle` | 无 `zhive.turn` / `zhive.subagent`；可能有 root `zhive.hook`（SessionStart / Notification）/ `zhive.rollback_point`（用户主动 fork） | engine 顶层 phase 自身**不开 span**——通过 `subscribe_phase()` 的 watch::Receiver 在 `phase/changed` 事件触发时 emit `info!("phase_changed", from=?, to=?)` 即可，不需要 phase span（phase 是状态而非工作单元） |
| `Turn` | `zhive.turn` 必 active；其中嵌 `zhive.tool_call` * N + `zhive.permission` * N + `zhive.hook(PreToolUse|PostToolUse|...)` * N | turn 是 span 的天然边界 |
| `Compaction` | `zhive.hook(PreCompact)` + `zhive.hook(PostCompact)`；**no** dedicated `zhive.compaction` span（D-014 没列） | TODO B9-1 见 §7 |
| `BranchSummary` | `zhive.rollback_point`（fork 时）+ `zhive.hook(?)`（A4 暂未列 BranchSummary 事件） | TODO B9-2 |
| `Retry` | `zhive.turn` 仍 active（在内部 backoff 循环上发 `warn!("turn_retry", attempt=N, backoff_ms=M)`） | retry 不开新 span；仅日志事件 |
| `SubagentSpawn` | `zhive.subagent` 必 active；其中嵌 父 `zhive.hook(SubagentStart)` 触发后；子 thread 自己的 `zhive.turn` 作为子 span | 父 `zhive.turn` 仍持有 |

### 2.3 instrument 推荐方式

D-014 没说"用 `#[instrument]` 还是手写 `span!`"。本 deliverable 定下：

- **`Engine::start_turn` / `Engine::spawn_subagent` / `HookHost::dispatch` 这种长函数**：用 `tracing::instrument!` 宏在函数内部手写 + `span.in_scope` / `instrument` await（async-trait 兼容性强，避免 `#[instrument]` proc-macro 在 generic + async-trait 上踩坑）。
- **小辅助方法（`PermissionReducer::evaluate` / tool call 派发）**：用 `#[tracing::instrument(skip(self, large_args), fields(tool_name = %name))]` 属性宏（async fn 直接支持，参 `tracing` docs `#[instrument]` 章节）。
- **统一硬约束**：禁止 `#[instrument(skip_all)]` 默认覆盖 self —— 必须显式列字段，避免 PII 泄漏（tool_input / prompt 字符串永远不进 span fields，只在 `debug!` 级日志按需打）。

---

## 3. 字段命名规约（OTel semconv 对齐 / R-6 风险落点）

### 3.1 命名总则

1. **wire 用 snake_case，span field 也用 snake_case**（与 OTel 一致，与 zhive Rust 内部 `thread_id` 同形）。
2. **优先使用 OTel semconv 已有 attribute name**，必要时加 `zhive.` 前缀作 zhive 自有扩展（命名空间隔离，避免与上游冲突）。
3. **每个 span 必含**：`session.id`（= zhive `thread_id`） —— 让所有 span 都能在 OTel backend 按 session 串起来。
4. **不上 wire 的字段**：`prompt / system_prompt / tool_input / tool_result.content / user_input.text / reasoning.text` —— 这些是 PII / 体积大，只在 `debug!` / `trace!` 级日志事件中按需 emit，不进 span field。
5. **`#[non_exhaustive]` enum 的 wire 取值**：用 `serde(rename_all = "snake_case")` 输出，span 里直接 `%enum_value` 写入（如 `?phase` 会得到 `Idle / Turn / ...`）。

### 3.2 zhive 字段 → OTel semconv 完整映射表

| zhive 字段 | 类型 | OTel semconv 对应 attribute | 是否进 span field | 落点 span | 备注 |
|---|---|---|---|---|---|
| `thread_id` | `ThreadId`（String） | `session.id` + `gen_ai.conversation.id` | ✅ 全部 6 个 span 都含 | all | 双重命名：OTel general 用 `session.id`，GenAI 用 `gen_ai.conversation.id`。zhive 决策：**span field 用 `session.id`**（更通用），仅在 GenAI inference span 额外写一份 `gen_ai.conversation.id`（即同值双写，OTel backend 两边都能查） |
| `turn_id` | `TurnId`（String） | 无标准对应 —— GenAI semconv 把 turn 视为 conversation 内的一次 inference call | ✅ `zhive.turn` / `zhive.tool_call` / `zhive.permission` | turn 内嵌 span | 用 zhive 自有 `zhive.turn.id`。**不**复用 `gen_ai.conversation.id`（语义不同：conversation = thread，turn 是其子单元） |
| `provider_name` | String（"openai" / "anthropic" / "local-ggml" / ...） | `gen_ai.provider.name` （新规约；旧规约叫 `gen_ai.system`） | ✅ `zhive.turn` / `zhive.subagent` | LLM 调用层 | OTel 最新版用 `gen_ai.provider.name`；早期文档为 `gen_ai.system`。zhive 跟最新 → `gen_ai.provider.name` |
| `model` | String（"gpt-4o" / "claude-sonnet-4-5" / ...） | `gen_ai.request.model` / `gen_ai.response.model` | ✅ `zhive.turn` / `zhive.subagent` | LLM 调用层 | 请求时填 `gen_ai.request.model`；响应回来若有 model_id 字段（如 OpenAI `gpt-4-0613`）追加 `gen_ai.response.model` |
| `operation` | "chat" / "tool_call" / "compact" / ... | `gen_ai.operation.name` | ✅ `zhive.turn` / `zhive.tool_call` | LLM / tool 层 | OTel 已列 `chat / generate_content / text_completion / execute_tool / embeddings`。zhive 用 `chat` for turn-level，`execute_tool` for tool_call |
| `tool_name` | String | `gen_ai.tool.name` | ✅ `zhive.tool_call` / `zhive.permission` | tool 层 | 直接对齐 |
| `tool_call_id` | String | `gen_ai.tool.call.id` | ✅ `zhive.tool_call` / `zhive.permission` | tool 层 | 直接对齐 |
| `agent_id` | Option\<String\>（A4 base 字段） | `gen_ai.agent.id`（GenAI agent semconv） | ✅ `zhive.subagent` / `zhive.hook`（subagent 内事件） | subagent 路径 | `None` 时不写 field（避免 `agent_id=null` 噪声） |
| `agent_type` | Option\<String\>（A4 base 字段） | `gen_ai.agent.name` | ✅ `zhive.subagent` | subagent 路径 | |
| `parent_tool_use_id` | Option\<String\>（A4 base 字段） | 无标准 —— zhive 自有 | ✅ `zhive.subagent` / `zhive.hook(SubagentStop)` | subagent 路径 | 用 `zhive.parent_tool_call_id`（命名与 OTel `gen_ai.tool.call.id` 风格一致） |
| `usage.input_tokens` | u64 | `gen_ai.usage.input_tokens` | ✅ `zhive.turn` close 时 set | LLM 层 | 直接对齐 |
| `usage.output_tokens` | u64 | `gen_ai.usage.output_tokens` | ✅ `zhive.turn` close 时 set | LLM 层 | 直接对齐 |
| `turn_status` | `TurnStatus`（4 态） | 部分进 `error.type` —— Failed / Interrupted 时 set `error.type = "turn_interrupted" / "turn_failed"` | ✅ `zhive.turn` close 时 set | turn 层 | OTel `error.type` 是 free-form 字符串；zhive 推荐值见 §3.3 |
| `engine_phase` | `EnginePhase`（6 态） | 无标准 —— zhive 自有 | 不进 span（是状态而非工作单元）—— 仅在 `phase/changed` 时 emit `info!("phase_changed", from = ?prev, to = ?next)` | n/a（log event） | TODO B9-3 |
| `hook_event_name` | String（来自 A4 14 case） | 无标准 —— zhive 自有 | 内嵌在 span name `hook.PreToolUse` 而非 field | `zhive.hook` | |
| `registered_by.id` | String（"builtin:filesystem" / "user:my-skill" / ...） | `code.namespace` 大致语义 | ✅ `zhive.hook` | hook 层 | 用 zhive 自有 `zhive.extension.id`（命名空间清晰）+ 双写 `code.namespace` 供 OTel backend 用 |
| `registered_by.source` | `ExtensionSource`（builtin / user / project / local / mcp） | 无标准 —— zhive 自有 | ✅ `zhive.hook` | hook 层 | `zhive.extension.source` |
| `permission_decision` | `PermissionDecision`（A3 四态：allow / deny / once / always） | 无标准 —— zhive 自有 | ✅ `zhive.permission` close 时 set | permission 层 | `zhive.permission.decision` |
| `permission_source` | "user" / "policy" / "inherited" | 无标准 | ✅ `zhive.permission` | permission 层 | `zhive.permission.source` |
| `rpc.method` | String（"session/prompt" / "session/cancel" / ...） | `rpc.method`（标准） | ✅ JSON-RPC server 入口（B4） —— 由 server 层在 dispatch 时开一个独立 `zhive.rpc` span 包裹整个 RPC 请求 | server 层 | TODO B9-4：B9 是否要把 RPC span 列入"6 个必覆盖"？D-014 没列。本 deliverable 倾向**不列入硬清单**——server 层 RPC span 由 B4 定义，B9 只规约字段名 |
| `rpc.system.name` | "jsonrpc" 常量 | `rpc.system.name=jsonrpc` | ✅ server 层 span 常量字段 | server 层 | |
| `jsonrpc.protocol.version` | "2.0" 常量 | `jsonrpc.protocol.version` | ✅ server 层 | server 层 | |
| `jsonrpc.request.id` | String / Number | `jsonrpc.request.id` | ✅ server 层 | server 层 | |
| `rpc.response.status_code` | i64（JSON-RPC error code） | `rpc.response.status_code` | ✅ server 层 close 时 set 当失败 | server 层 | |
| `rollback_id` | String | 无标准 | ✅ `zhive.rollback_point` | rollback 层 | `zhive.rollback.id` |
| `rollback_reason` | "user_fork" / "auto_branch_summary" / ... | 无标准 | ✅ `zhive.rollback_point` | rollback 层 | `zhive.rollback.reason` |

### 3.3 `error.type` zhive 推荐取值表

OTel `error.type` 是 free-form string；zhive 规约下面 8 个值（其余 fallback `_OTHER`）：

| zhive 场景 | `error.type` 值 |
|---|---|
| Turn 被用户中断 | `turn_interrupted` |
| Turn LLM provider 不可恢复错误 | `turn_failed` |
| Tool call 失败（tool 返回 error 或 timeout） | `tool_call_failed` |
| Permission denied（reducer 拒绝） | `permission_denied` |
| Hook callback panic / non-zero exit | `hook_failed` |
| Subagent spawn 失败 | `subagent_spawn_failed` |
| Storage commit 失败 | `storage_failed` |
| JSON-RPC dispatch error | `rpc_error` |
| 其余 | `_OTHER`（OTel 标准 fallback） |

### 3.4 命名禁忌（防 R-6 后悔）

- ❌ `agent_status` —— codex 用，但 OTel 没有 "agent_status" 标准属性；改用 `gen_ai.agent.name` 或自有 `zhive.engine.phase`
- ❌ `model_id` —— OTel 用 `gen_ai.request.model` / `gen_ai.response.model`，要带 `request`/`response` 限定
- ❌ `session` 裸字 —— 与 OTel `session.id` 冲突歧义；用 `thread_id` 或 `session.id` 全名
- ❌ `user_id` —— Phase 1 暂不上 wire（PII；待 Phase 3 加 auth 时再起 `user.id` OTel 标准属性）
- ❌ `prompt` / `text` / `content` 直接进 span field —— 体积 + PII，只在 `trace!` 级日志事件按需 emit

---

## 4. 日志级别约定（D-014 没规定，本 deliverable 定下）

### 4.1 5 级使用守则

| 级别 | 触发条件 | zhive 例子 | 是否进生产 default |
|---|---|---|---|
| `error!` | **不可恢复**错误 / 进程级失败 / 数据完整性损坏风险 | LLM provider 5xx 不可重试、Storage commit 失败、JSON-RPC framing 解析失败、`unwrap_or_else(\|e\| error!(...))` 等 fallback 路径 | ✅ default `RUST_LOG=info` 时仍打 |
| `warn!` | **可恢复**异常 / 用户应该知道但 zhive 自己能处理 | Turn retry（每次 retry 发一条 warn）、Permission denied、Hook callback timeout（在 graceful 超时阈值内）、Tool call 失败但 LLM 还能继续 | ✅ default 打 |
| `info!` | **生命周期里程碑** / 不频繁的状态变化 | Engine spawn / shutdown、Thread 创建 / 关闭、`phase/changed`（每次 EnginePhase transition）、Subagent spawn / final message 回流、SessionStart hook、`turn/started` / `turn/completed`（一对 / turn） | ✅ default 打 |
| `debug!` | **每 turn / 每 tool_call 内部步骤** / 排查问题时需要的细节 | tool_call dispatch 前的参数预览（trim 到 200 char）、reasoning chunk 计数、`registered_by` 解析过程、permission reducer 评估的中间态、watch::Receiver lag | 默认关；`RUST_LOG=zhive=debug` 开 |
| `trace!` | **逐 event / 逐 chunk** / 含完整 PII 数据 / 性能热点 | 每个 reasoning chunk 字面内容、每个 LLM streaming SSE frame、完整 tool_input / tool_result JSON、channel send/recv 时序 | 默认关；`RUST_LOG=zhive=trace` 开 |

### 4.2 关键示例

```rust
// info! —— turn 生命周期里程碑
tracing::info!(
    session.id = %thread_id,
    zhive.turn.id = %turn_id,
    gen_ai.operation.name = "chat",
    gen_ai.request.model = %model,
    "turn started"
);

// warn! —— 可恢复 retry
tracing::warn!(
    session.id = %thread_id,
    zhive.turn.id = %turn_id,
    zhive.retry.attempt = attempt,
    zhive.retry.backoff_ms = backoff.as_millis() as u64,
    error.type = "turn_retry",
    "turn retry"
);

// error! —— 不可恢复
tracing::error!(
    session.id = %thread_id,
    zhive.turn.id = %turn_id,
    error.type = "storage_failed",
    error = %e,
    "rollout flush failed; data may be lost"
);

// debug! —— 排查细节（tool_input trim）
tracing::debug!(
    session.id = %thread_id,
    gen_ai.tool.name = %tool_name,
    gen_ai.tool.call.id = %tool_call_id,
    tool_input_preview = %trim_to(input_str, 200),
    "tool_call dispatched"
);
```

### 4.3 不打日志的反例

- ❌ 在 hot loop 里 `info!("got chunk")` —— 应 `trace!`
- ❌ 把 user prompt 完整 echo 到 `info!` —— PII，应 `debug!` + trim
- ❌ 用 `eprintln!` —— 全部走 tracing；CLAUDE.md 红线"用 `?` + thiserror"暗含"日志走 tracing"

---

## 5. tracing-subscriber 初始化点

### 5.1 候选位置

| 候选 | 位置 | 优点 | 缺点 |
|---|---|---|---|
| A. CLI 入口（`zhive-cli/src/main.rs`） | bin crate `fn main()` 第一行 | 与 CLAUDE.md "禁止 unwrap" 约束契合（CLI 是 bin，可在 main 顶部 unwrap subscriber init）；用户可通过 CLI flag / env 配置 `RUST_LOG` | 库 crate 不 init；如果别人把 zhive-core 当 lib 用（如未来 IDE 嵌入），subscriber 必须 IDE 自己起 |
| B. core 入口（`zhive-core::Engine::spawn()`） | `Engine::spawn(config)` 内部 try-init | 库自带 default subscriber，开箱即用 | **违反** tracing 生态惯例：lib 不应 init global subscriber（参 `tracing` crate docs §"Library Authors"）；多 init 会 panic |
| C. 提供 `Engine::run_with_subscriber()` builder | `EngineBuilder::with_subscriber(impl Subscriber)` 或 `EngineConfig.subscriber: Option<Box<dyn Subscriber>>` | 灵活：CLI 自己装一个 fmt，IDE 嵌入时装 OTel | 增加 builder 复杂度；首次实装时不必要 |

### 5.2 决策：**A（CLI 入口）+ 留 C 扩展位**

**选项 A**：subscriber 初始化在 `crates/zhive-cli/src/main.rs` 第一行（bin crate）+ `crates/zhive-server/src/main.rs`（如果将来 server 独立 bin）。

**理由**：
1. **tracing 生态惯例**：lib crate（zhive-core / zhive-proto / ...）**绝对不**调用 `tracing_subscriber::registry().init()`——参 [tracing docs §Setting up a subscriber](https://docs.rs/tracing/latest/tracing/#in-libraries)：lib 只发 events / 开 span，subscriber 由 bin 装。
2. **D-014 已锁定 `fmt + env-filter`**：CLI bin 初始化代码可以简到 5 行（见 §5.3）。
3. **OTel feature gate（D-014）**：当 cargo feature `otel` 开启时，CLI bin **额外**注册 `tracing-opentelemetry` layer；core 完全不感知。

**留扩展位 C**：core 提供 `Engine::set_dispatch_for_tests(...)` 一类 test-only 钩子，让 nextest 用例可装 in-memory subscriber 验证 span 触发（参 `tracing` crate `tracing::dispatcher::with_default`）—— 但**这是 test-only API，不暴露给用户**。Phase 1 不实装 builder C，先用 A 跑通。

### 5.3 CLI bin 初始化代码草图（不进 crates/）

```rust
// zhive-cli/src/main.rs
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("zhive=info,warn"));

    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        // 关键：tracing-subscriber 默认会 emit field PII（如 tool_input_preview）
        // 这里依赖每个 emit point 自己做 trim/skip，不在 fmt 层全局过滤
        ;

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    #[cfg(feature = "otel")]
    let registry = registry.with({
        let tracer = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(/* OTLP gRPC/HTTP exporter; Phase 1 不实装 */)
            .build()
            .tracer("zhive");
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    registry.init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    // ...
}
```

**编译性约束**：
- `tracing_subscriber::EnvFilter::try_from_default_env()` 失败时回落 `zhive=info,warn`（不 `unwrap` —— CLAUDE.md 红线）
- `tracing_subscriber::Registry::init()` 在多次调用时 panic；CLI bin 仅一次 entry 调用，安全
- OTel layer 在 cargo feature gate 后；Phase 1 主路径不引 `tracing-opentelemetry` / `opentelemetry-sdk` crate 依赖（CLAUDE.md "禁止新增 dependency"，仅作 Phase 2 扩展位）

---

## 6. 关键问题逐条作答

| # | 问题 | 答案（≤ 8 行） |
|---|---|---|
| 1 | 6 个必覆盖 span 的 instrument 位置 + 与 B1 EnginePhase 6 态对应关系 | §2 完整矩阵。要点：`zhive.turn`（`Engine::start_turn` 入口 → `turn/completed`，对应 `EnginePhase::Turn`）/ `zhive.hook`（B5 `HookHost::dispatch` 单一入口，跨所有 phase）/ `zhive.subagent`（`Engine::spawn_subagent`，`SubagentSpawn` phase）/ `zhive.permission`（A3 reducer + 反向 RPC，多在 `Turn` 内）/ `zhive.tool_call`（LLM stream 解析 tool_call item 时打开，`Turn` 内）/ `zhive.rollback_point`（B2 rollback 触发点，`Idle/BranchSummary/Turn` 内都可能）。EnginePhase 自身**不开 span**，仅用 `phase/changed` event + `info!` 记录。 |
| 2 | span 字段命名是否对齐 OTel semconv？ | **对齐**。规约总则见 §3.1，逐项映射表见 §3.2。关键决策：(a) `thread_id` 双写 `session.id` + `gen_ai.conversation.id`；(b) `tool_name / tool_call_id` 直接用 `gen_ai.tool.name` / `gen_ai.tool.call.id`；(c) `provider_name` 用 OTel 最新 `gen_ai.provider.name`（不用旧 `gen_ai.system`）；(d) `turn_id` 无 OTel 对应，用 zhive 自有 `zhive.turn.id`；(e) RPC 层用 `rpc.system.name=jsonrpc` / `rpc.method` / `jsonrpc.protocol.version` / `jsonrpc.request.id` / `rpc.response.status_code`；(f) zhive 自有扩展统一加 `zhive.` 前缀。 |
| 3 | 何时 `error!` / `warn!` / `info!` / `debug!` / `trace!`？ | §4 完整表 + 示例。要点：error = 不可恢复 / 进程级失败；warn = 可恢复异常（含 retry / permission denied / hook timeout）；info = 生命周期里程碑（engine spawn / phase/changed / turn lifecycle / subagent lifecycle）；debug = 每 turn / 每 tool_call 内部步骤（含 trim 过的 tool_input preview）；trace = 逐 chunk / 完整 PII。default `RUST_LOG=info,warn` 打 error+warn+info，debug/trace 需显式开。 |
| 4 | tracing-subscriber 初始化在哪一层？ | **CLI bin 入口**（`zhive-cli/src/main.rs::init_tracing()`），core / proto / 其他 lib crate **绝对不**调用 `registry.init()`（lib 不 init subscriber 是 tracing 生态惯例，参 tracing docs §"In Libraries"）。OTel layer 通过 cargo feature `otel` gate 在 CLI bin 处条件注册，core 完全不感知。留扩展位 C（`Engine::set_dispatch_for_tests` test-only 钩子）供 nextest 用例装 in-memory subscriber 验证 span 触发，但**不暴露公开 builder**。详见 §5。 |

---

## 7. 未决项（回流到 plan §9 风险表）

> TODO(开放项 B9-1)：`EnginePhase::Compaction` 期间 D-014 没列 `zhive.compaction` 专属 span，仅靠 `zhive.hook(PreCompact)` + `zhive.hook(PostCompact)` 两个独立 span 串联。如果 compaction 内部还有 LLM 调用（语义压缩需要 model 推理），span 父子关系会断（PreCompact / PostCompact 是两个独立 root）。建议落地时补一个 `zhive.compaction` span 作为容器，覆盖整个 Compaction phase；D-014 字面写"至少 6 个 span"，加新 span 不破坏。

> TODO(开放项 B9-2)：`EnginePhase::BranchSummary` 同 B9-1 —— A4 deliverable 14 hook 事件里没列 `BranchSummary` 类，导致这个 phase 期间唯一 span 是 `zhive.rollback_point`（仅在 fork 瞬间）。建议补 `zhive.branch_summary` 容器 span + A4 新增 `PreBranchSummary / PostBranchSummary` reserved hook（与 A4 已经建议的 `PostCompact` 一起回流到 `decision-diffs.md`）。

> TODO(开放项 B9-3)：`EnginePhase` 是状态而非工作单元，本 deliverable §2.2 决策"不开 phase span，用 `phase/changed` log event"。但 OTel backend 通常按 span 时间轴可视化，没有 phase span 时无法在 timeline 上看到"engine 此刻在 Idle vs Turn vs Compaction"。备选：在 watch::Receiver<EnginePhase> 变化时开 / 关一个 `zhive.phase.{name}` 长 span。倾向**不实装**——会让 span 数翻倍，且与 `zhive.turn` 高度重叠。落地时观察 B9-1/B9-2 之后是否还需要。

> TODO(开放项 B9-4)：RPC 层 span（含 `rpc.method` / `jsonrpc.request.id`）是否进"D-014 必覆盖清单"？D-014 字面只列 6 个业务 span。本 deliverable §3.2 把 RPC 字段命名规约写下，但 instrument 位置（推荐 B4 在 server dispatcher 入口）属于 B4 deliverable 范畴。建议 B4 落地时确认"RPC span 与 zhive.turn 父子关系"（zhive.turn 由 rpc `session/prompt` 入口开启，应是 RPC span 的子 span）。

> TODO(开放项 B9-5)：`broadcast::Sender<EngineEvent>` 容量 1024（B1 §6）+ tracing 强制覆盖事件流可能造成 channel lag。建议在 B9 实装后用真实 turn 量测一次（每 turn event 数 × 客户端订阅数 × 并发 turn 数），如有 lag 调容量或上 watch::Sender<latest_snapshot> 模式。本 deliverable 不解决，仅作回流到 B1 TODO B1-6。

> TODO(开放项 B9-6)：`tracing-opentelemetry` 在 Phase 1 不装（D-014 + R-6），但本 deliverable §5.3 草图里 `#[cfg(feature = "otel")]` 块需要在 Cargo.toml 写 optional dependency。**这意味着 Phase 1 要在 Cargo.toml 加 `tracing-opentelemetry` / `opentelemetry-sdk` 作为 optional dep**——但不进默认 features。是否触发 CLAUDE.md "禁止新增 dependency" 红线需要用户确认。备选：Phase 1 完全不在 Cargo.toml 提 OTel crate 名，feature gate 代码用 stub trait + 文档注释占位，等 Phase 2 装时再加 dep。**倾向后者**（更严守红线）。

> TODO(开放项 B9-7)：日志事件字段命名（`tracing::info!(field = value, ...)`）与 span field 命名是否要严格一致？本 deliverable §3 / §4 示例用同名（如 `session.id` 在 span field 与 log event 字段同名）。但 tracing macro 字段名带 `.` 需要 raw identifier 或字符串 key（`r#"session.id"#` 形式），人体工学差。备选：log event 用 `session_id` snake_case，span field 用 `session.id`，由 OTel exporter layer 在 mapping 时翻译（`tracing-opentelemetry` 自动把 `.` 翻成 OTel attribute）。**倾向 log event `session_id` 简写 + span field `session.id` 标准**——双轨命名，落地时在 B9 实装文档里给 mapping 表。

---

## 8. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中 OTel URL section + B1/A1/A4 行号引用）
- [x] 不动 `crates/` 源码（所有草图均在本 markdown 内，且 `init_tracing()` 是 bin 入口示例不进 lib）
- [x] 不改 `research/99-decisions/`（仅引用 D-014 + 在 §7 未决项 B9-1/B9-2 提建议回流 `decision-diffs.md`）
- [x] 不 `git pull`
- [x] **6 个 span 矩阵**：§2.1 表
- [x] **OTel semconv 字段映射表**：§3.2 表
- [x] **日志级别约定**：§4 表 + 示例
- [x] **subscriber 初始化点选型**：§5.2 决策 A + 留扩展位 C
- [x] **关键问题 4 条逐条作答**：§6
- [x] **未决项**：§7 共 7 条 `> TODO(开放项 B9-N)`
- [x] R-6 风险落地：字段命名严格对齐 OTel GenAI / RPC / Session / Error 4 域；自有扩展统一加 `zhive.` 前缀；§3.4 列命名禁忌

— B9 deliverable end —
