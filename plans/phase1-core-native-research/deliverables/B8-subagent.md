---
task: B8
title: Subagent 调度（fresh window / only final / 禁递归）
date: 2026-05-28
status: draft
depends_on:
  - B1 deliverable (Engine actor pattern + EnginePhase 6 态含 `SubagentSpawn` + channel 拓扑)
  - B6 deliverable (父子 reducer 双调 + `subagent_decision_tx: mpsc::Sender<PermissionDecision>` in-process 传值 + BypassPermissions short-circuit)
  - A3 deliverable (`SubagentDefinition` wire 形态 + `narrowed_into` + `allow_subagent_spawn: bool` 默认 false)
  - A5 deliverable (`disable_in_subagent` 字段)
  - D-008 (Subagent permission inheritance / reverse-RPC / fresh context window)
references:
  - https://code.claude.com/docs/en/agent-sdk/subagents              ("Each subagent runs in its own fresh conversation. Intermediate tool calls and results stay inside the subagent; only its final message returns to the parent." + "Subagents cannot spawn their own subagents. Don't include `Agent` in a subagent's `tools` array." + AgentDefinition 字段表 + "The parent receives the subagent's final message verbatim as the Agent tool result")
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §2.1 (74-97) `EnginePhase::SubagentSpawn` 第 6 态定义
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §2.3 (148-155) 转换矩阵 `Turn ↔ SubagentSpawn → Turn`
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (245-274) `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `Engine: Clone` actor handle
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (293-304) `ActiveTurn { item_tx: mpsc::Sender<Item> }` 单 producer 单 consumer
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (332-352) `Submission::SpawnSubagent { parent_thread_id, spec, reply: oneshot::Sender<Result<ThreadId, _>> }`
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (450-457) `Engine::spawn_subagent` 公开签名
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (306-316) `TurnKind { Regular, Subagent, Review }`
  - plans/phase1-core-native-research/deliverables/B6-permission-reducer.md  §3.1 (138-217) 父子调用图 ASCII
  - plans/phase1-core-native-research/deliverables/B6-permission-reducer.md  §3.2 (219-227) phase 切换"毫秒级仪式"语义
  - plans/phase1-core-native-research/deliverables/B6-permission-reducer.md  §3.3 (229-237) BypassPermissions short-circuit（解 A3-O4）
  - plans/phase1-core-native-research/deliverables/B6-permission-reducer.md  §7 TODO B6-O6 (498) child Defer ⇒ 父子两层 suspended notification
  - plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md §7.1 (457-462) 三大不变式（父→子单向 / 子可缩窄不可放大 / reducer 双调）
  - plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md §7.2 (465-518) `narrowed_into` + `ScopeError::RecursionForbidden`
  - plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md §7.3 (521-553) `SubagentDefinition` wire 形态 + 无 `inherited_permissions` 字段
  - plans/phase1-core-native-research/deliverables/A5-extension-manifest.md §（173 / 189 / 497）`disable_in_subagent: bool` 字段 + A5-O6 是否扩展到 extension+tools+hooks
---

> **设计衔接警告**：本 deliverable 承接 B1 actor pattern + B6 reducer 双调，**对 child engine 选 "复用同一 `EngineInner.threads` HashMap 内一个新 ThreadHandle" 而非新进程/新 `Engine` 实例**。这与 B6 §3.2 "subagent 不是跨进程 thread，child engine 与 parent engine 共享 EngineInner.threads HashMap，只在 phase / cancel 隔离" 一致。"fresh context window" 在此**不是新 Engine**，而是 **新 ThreadHandle + 新 active_turn + 空 history**（B1 §4 `ThreadHandle` 即天然 fresh）。

---

## 1. 参考点清单

下面所有论断均回指此清单，逐条按 `路径:行号` 锚定。

| 主题 | 路径 | 行号 |
|---|---|---|
| Claude Code: subagent fresh context + only final message | `https://code.claude.com/docs/en/agent-sdk/subagents` | "Each subagent runs in its own fresh conversation. Intermediate tool calls and results stay inside the subagent; only its final message returns to the parent." |
| Claude Code: 禁递归（subagent 不能 spawn subagent） | 同上 | "Subagents cannot spawn their own subagents. Don't include `Agent` in a subagent's `tools` array." |
| Claude Code: final 由 Agent tool result 携带（verbatim） | 同上 | "The parent receives the subagent's final message verbatim as the Agent tool result" |
| Claude Code: parent → child 单一通道 = Agent tool prompt string | 同上 | "The only channel from parent to subagent is the Agent tool's prompt string" |
| Claude Code: AgentDefinition.tools / disallowedTools / permissionMode / model / mcpServers / maxTurns / skills / memory / background / effort | 同上 | AgentDefinition table |
| Claude Code: 旧 tool 名 `Task`，v2.1.63 后改 `Agent`；`parent_tool_use_id` 字段表明该 message 来自 subagent 上下文 | 同上 | "Detecting subagent invocation" 章 |
| zhive B1 `EnginePhase::SubagentSpawn`（第 6 态） | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | §2.1 (74-97) |
| zhive B1 phase 转换矩阵：`Turn → SubagentSpawn → Turn`（不允许其他来源） | 同上 | §2.3 (148-155) |
| zhive B1 `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `Engine: Clone`（actor pattern） | 同上 | §4 (245-274) |
| zhive B1 `Submission::SpawnSubagent { parent_thread_id, spec, reply: oneshot::Sender<Result<ThreadId, _>> }` | 同上 | §4 (348-348) |
| zhive B1 `Engine::spawn_subagent(parent_thread_id, spec) -> Result<ThreadId>` | 同上 | §4 (450-457) |
| zhive B1 `TurnKind = Regular \| Subagent \| Review`（Subagent kind 已留位） | 同上 | §4 (306-316) |
| zhive B1 `ActiveTurn.item_tx: mpsc::Sender<Item>`（单 producer / 单 consumer，turn 内事件流） | 同上 | §4 (293-304) |
| zhive B6 父子调用图（含 `parent.subagent_decision_tx.send(fold_child)` in-process channel） | `plans/phase1-core-native-research/deliverables/B6-permission-reducer.md` | §3.1 (138-217) |
| zhive B6 SubagentSpawn phase 切换语义（"毫秒级仪式"，不是 child 运行时态） | 同上 | §3.2 (219-227) |
| zhive B6 BypassPermissions short-circuit（child hook 仍 dispatch，返回值替换为 Allow） | 同上 | §3.3 (229-237) |
| zhive B6 child Defer ⇒ 父子双层 turn/suspended | 同上 | §7 TODO B6-O6 (498) |
| zhive A3 三大不变式（父→子单向 / 缩窄不放大 / reducer 双调） | `plans/phase1-core-native-research/deliverables/A3-permission-streaming-subagent.md` | §7.1 (457-462) |
| zhive A3 `narrowed_into` 内 `child.allow_subagent_spawn == true ⇒ Err(ScopeError::RecursionForbidden)` | 同上 | §7.2 (498-501) |
| zhive A3 `SubagentDefinition` wire（无 `inherited_permissions` 字段，靠 `Option` 缺省） | 同上 | §7.3 (521-553) |
| zhive A5 `disable_in_subagent: bool` 字段（A5-O6 是否扩展到 extension+tools+hooks） | `plans/phase1-core-native-research/deliverables/A5-extension-manifest.md` | 173 / 189 / 497 |

---

## 2. Subagent spawn 形态决定（关键问题 #1 完整答案）

### 2.1 决定：**新 ThreadHandle**（同一 `EngineInner.threads` HashMap 下），**不**新 `Engine` 实例

**两个备选**：

| 备选 | 描述 | 选 / 不选 | 理由 |
|---|---|---|---|
| **A. 新 `Engine` 实例** | child 是独立 `Engine`，独立 `EngineInner.threads / phase_tx / event_bus / hook_host / provider / storage / reverse_rpc` | **不选** | (a) 新 Engine 要复制 5 个 `Arc<dyn ...>` 注入（B1 §4 line 263-274），且 child storage / reverse_rpc 必须共享 parent 的 sink，复制后等价于"假独立"；(b) `Engine::shutdown` 语义会被污染——shutting down child 会触发 child engine 的 shutdown CancellationToken，但实际需要保留 parent；(c) 与 B1 actor pattern 衔接成本高：reverse-RPC 走向需要 child engine 内额外建一个 RPC sink 路由器，工程复杂度爆炸 |
| **B. 同 `Engine`，新 `ThreadHandle`** | child thread 注入到 `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>`（B1 §4 line 253），与普通 thread **同位置** | **选** | (a) "fresh context window" 等价于"新 ThreadHandle + 新 active_turn + 空 thread history"，B1 §4 `ThreadHandle.thread: Arc<RwLock<Thread>>` 天然 fresh；(b) reverse-RPC / hook_host / provider 都复用 parent engine 的，单一 sink；(c) parent / child 同进程同 engine，`fold_child` 通过 in-process `mpsc::Sender<PermissionDecision>` 传值（B6 §3.2 已锁）；(d) Claude Code 文档 "Each subagent runs in its own fresh conversation" 字面是 "conversation" 而非 "agent instance"——zhive 的 thread = 一个对话，正好对应 |

### 2.2 与 B1 actor pattern 衔接

- **新 thread 注入**：`EngineInner.threads.write().await.insert(child_tid, Arc::new(child_handle))`（B1 §4 line 253）—— **与普通 thread 创建同代码路径**，唯一区别是 `TurnKind = Subagent` + child handle 持有 `parent_thread_id: Option<ThreadId>` 指针
- **child agent loop task**：每个 thread 一个独立 agent loop task（B1 §6.3 line 749-750 "每个 thread 一个独立 agent loop task"），child thread 也照此 spawn，**无特殊路径**
- **fresh context**：child `ThreadHandle.thread: Arc<RwLock<Thread>>` 是新对象，`Thread.items: Vec<Item>` 空 —— context window 自然 fresh。Claude Code 文档 "What subagents inherit" 表列出 child 收到的只有 `AgentDefinition.prompt` + Agent tool 的 prompt string + project CLAUDE.md + tools subset，**不含** parent conversation —— zhive 这套构造直接吻合
- **cost 分析**：方案 B 增量代价仅 = `ThreadHandle` 一份（约 200-500 bytes 的句柄 + 一个 agent loop task）。方案 A 增量代价 = 整个 `EngineInner` 一份（含 7 个 `Arc<dyn ...>`） + spawn dispatcher loop —— **B 比 A 至少便宜一个数量级**

### 2.3 child ThreadHandle 额外字段（B1 §4 ThreadHandle 上加）

```rust
pub(crate) struct ThreadHandle {
    thread_id: ThreadId,
    active_turn: Mutex<Option<ActiveTurn>>,
    cancel: CancellationToken,
    thread: Arc<RwLock<Thread>>,
    sub_tx: mpsc::Sender<Submission>,
    _loop_handle: AbortOnDropHandle<()>,

    // ===== B8 新增字段 =====
    /// 该 thread 是否是 subagent；Some(parent_tid) ⇒ child；None ⇒ 顶层 thread
    /// 用于：(a) 禁递归检查；(b) 决定 `EngineEvent` fan-out 时是否打 `parent_tool_use_id` 标记
    parent_thread_id: Option<ThreadId>,

    /// child → parent 的 permission decision 反向通道（B6 §3.2 line 224）
    /// 仅 child thread 持有；spawn 时由 parent 注入 sender，parent 持有对应 receiver
    /// None ⇒ 顶层 thread，无父
    subagent_decision_tx: Option<mpsc::Sender<PermissionDecision>>,

    /// child agent loop 触发的"父侧 Agent tool result" 回流通道
    /// **专门承载 final message**（见 §6）；child final 落 Item 时把 final 文本 send 进去
    /// parent agent loop 在 `Submission::SpawnSubagent` 的 reply oneshot 之后转入"等 final"状态
    /// 用 mpsc 不用 oneshot：理论上 child 在 final 之后还会有 trailing item（如 `turn/completed` notification），mpsc 允许多帧
    /// 但 final 只一帧，多余帧由 parent 丢弃（§6.3）
    subagent_final_tx: Option<mpsc::Sender<SubagentFinalEvent>>,
}
```

> TODO(开放项 B8-1)：`subagent_decision_tx` 与 `subagent_final_tx` 两条 channel 是否合并为单一 `subagent_event_tx: mpsc::Sender<SubagentInboundEvent>` enum？倾向**保留分离** —— 二者生命周期不同：decision 在每个 tool_call 触发，final 仅一次；分开让 parent 侧 select! 分支清晰。落地时 B6 / B8 实现者可二选一。

---

## 3. Subagent spawn 流程图（ASCII）

> 衔接 B6 §3.1 但聚焦 spawn 仪式 + final 回流。B6 图聚焦 reducer 父子调用；本图聚焦"创建 → 运行 → final 回流 → 销毁"。

```text
PARENT engine (single EngineInner instance, shared)               CHILD engine = same EngineInner, NEW ThreadHandle
─────────────────────────────────────────────                     ────────────────────────────────────────────────
EnginePhase = Turn                                                 (尚未存在)
ThreadHandle(parent_tid).active_turn = Some(ActiveTurn)
                │
                │ LLM stream 返回 tool_call(name="Agent",
                │   input={ subagent_type, prompt, ... })
                │
                │ (per Claude Code docs: parent → child 单一通道
                │  是 Agent tool 的 prompt string，含路径/错误/决策)
                ▼
   ┌──────────────────────────────────────┐
   │ parent agent loop:                   │
   │ 1. 查 AgentDefinition (by name)     │  ← `EngineConfig.subagents: HashMap<String, SubagentDefinition>`
   │ 2. parent_scope = parent.scope       │
   │ 3. compute child_scope = inherit +   │  ← A3 §7.2 narrowed_into 的反向构造
   │    apply SubagentDefinition.tools /  │
   │    disallowed_tools / permission_mode│
   │ 4. parent_scope.narrowed_into(&child_scope)? │  ← 检查缩窄合法 + 禁递归（A3 §7.2 line 498-501）
   │ 5. 检查 parent_thread_id.is_some() ? │  ← **禁递归 #1（spawn 入口拦）**：parent 已是 child ⇒ 拒
   │    Err(EngineError::RecursionForbidden)│   （§4.2）
   │ 6. Submission::SpawnSubagent {...}   │
   └──────────────────────────────────────┘
                │
                │ phase: Turn → SubagentSpawn  (B1 §2.3)
                │ emit PhaseTransition hook    (B1 §6.7)
                │ emit phase/changed notification
                ▼
   ┌──────────────────────────────────────┐
   │ Engine dispatcher:                   │
   │ - 创建 child ThreadId (uuid v7)      │
   │ - 创建 child ThreadHandle:           │
   │     parent_thread_id = Some(parent_tid)
   │     subagent_decision_tx = Some(...)
   │     subagent_final_tx = Some(...)
   │     thread = new empty Thread        │  ← fresh context window
   │     active_turn = None               │
   │ - threads.write().insert(child_tid, ...)│
   │ - spawn child agent loop task         │
   │ - 把 SubagentDefinition.prompt +     │
   │   Agent tool input.prompt 拼成       │
   │   child initial UserInput            │
   │ - child_engine.start_turn(child_tid, [prompt])│  ← 同入口，无特殊路径
   └──────────────────────────────────────┘
                │
                │ phase: SubagentSpawn → Turn  (B1 §2.3)
                │ 注：SubagentSpawn 仅持续毫秒级（B6 §3.2 line 227）
                │
                │ (parent agent loop 阻塞等 subagent_final_rx.recv())
                ▼                                                       │
   ┌──────────────────────────────────────┐                            ▼
   │ parent ActiveTurn 继续，但 LLM       │                ┌───────────────────────────┐
   │ stream 已 yield 给 Agent tool 等结果 │                │ child ThreadHandle:        │
   │ pending_tool_call.await =            │                │ - active_turn = Some(...)  │
   │   subagent_final_rx.recv()           │                │ - phase 字段不存在（per-thread phase 推到 B1-2 未决）│
   └──────────────────────────────────────┘                │ - child agent loop:        │
                                                            │   inner Turn lifecycle 完整跑│
                                                            │   含 tool_call / reasoning  │
                                                            │   含 permission/request    │
                                                            │   含 hook dispatch         │
                                                            │   含 follow_up / steer (?) │  ← §7 未决 B8-2
                                                            └───────────────────────────┘
                                                                       │
                                                                       │ ... child Turn 内多轮 tool_call ...
                                                                       │
                                                                       │ child LLM 最终一帧
                                                                       │ Item::AgentMessage { final=true } 落 item_tx
                                                                       ▼
                                                            ┌───────────────────────────┐
                                                            │ child item appender:       │
                                                            │ - 持久化（B3 JSONL）       │
                                                            │ - emit ItemAppended event  │
                                                            │ - **若 item.final ⇒ 同时**:│
                                                            │     subagent_final_tx.send(│
                                                            │       SubagentFinalEvent { │
                                                            │         text: item.text,   │
                                                            │         child_tid })       │
                                                            └───────────────────────────┘
                                                                       │
                ┌──────────────────────────────────────────────────────┘
                ▼
   ┌──────────────────────────────────────┐
   │ parent agent loop 解 await:          │
   │ subagent_final_rx.recv() = Ok(final) │
   │ - 构造 ToolResult { content: final.text, parent_tool_use_id: child_tid 标记 }
   │ - 落 Item::ToolResult 进 parent ActiveTurn.item_tx
   │ - parent LLM 下一轮请求把该 ToolResult 当作 Agent tool 的返回值
   └──────────────────────────────────────┘
                │
                │ (optional) child Turn 完成 ⇒
                │   child ThreadHandle 是否 drop？
                │   倾向 **保留**（per Claude Code "Resuming subagents"
                │   语义：subagent transcripts persist），让 parent 可
                │   再次调 Agent tool with `agent_id` 续命
                │   （§5 final + §7 未决 B8-3）
                ▼
   ┌──────────────────────────────────────┐
   │ parent ActiveTurn 继续；child handle  │
   │ 留在 threads HashMap，状态 = idle      │
   │ 下一次 parent LLM 调 Agent tool 同名  │
   │ 可走 "resume" 路径（B8-3 未决）       │
   └──────────────────────────────────────┘
```

---

## 4. 禁递归实现位置（关键问题 #2 完整答案）

### 4.1 决定：**spawn 入口拦（runtime 检查）+ schema 默认禁（serde default false）双层**

| 层 | 位置 | 检查内容 | 错误类型 |
|---|---|---|---|
| **L1 schema 默认** | `SubagentDefinition.allow_subagent_spawn: bool` 默认 `false`（A3 §7.3 line 547-548 `#[serde(default)]`） | client 构造 SubagentDefinition 时若不显式打开 ⇒ 该 subagent 无 Agent tool | 静态：tools[] 内没 `Agent` |
| **L2 scope narrow 时检查** | `PermissionScope::narrowed_into(child)` 内 `if child.allow_subagent_spawn { Err(ScopeError::RecursionForbidden) }` （A3 §7.2 line 498-501） | parent 构造 child scope 时若 child 试图开 `allow_subagent_spawn = true` ⇒ 拒 | `ScopeError::RecursionForbidden` |
| **L3 spawn 入口 runtime 拦** | `Engine::spawn_subagent` 公开方法首行：检查 `threads.read()[&parent_thread_id].parent_thread_id.is_some() ⇒ Err(EngineError::RecursionForbidden)` | 父 thread 自身是 child ⇒ 拒 spawn | `EngineError::RecursionForbidden` |

### 4.2 为什么要三层（"前者简单后者更严"的折中）

- **L1 schema 默认 false**：与 Claude Code 文档 "Subagents cannot spawn their own subagents. Don't include `Agent` in a subagent's `tools` array" 字面对齐。这是**第一道防线**（用户错误配置不会导致灾难）
- **L2 narrowed_into**：A3 已把 `allow_subagent_spawn: bool` 当 narrow 检查项。这一道防 client 显式打开 `allow_subagent_spawn=true` 的情况（即使 schema 允许，子缩窄规则也拒）
- **L3 runtime 拦**：万一前两道被绕（如未来扩展 default true 或某 client 跳过 narrow check），最后一道在 spawn 入口看 `parent_thread_id.is_some()` ——**对运行时事实的检查**，无法被静态配置绕过

**"前者简单"**：L1 是 0 行代码（仅 `#[serde(default)]`）。"**后者更严**"：L3 在 runtime 强制，无法绕过。zhive 选**三层并行**（深度防御 / defense-in-depth），任一层失效不会导致递归。

### 4.3 报错语义

- L1 失效：subagent 没有 Agent tool ⇒ LLM 不会调用 Agent tool ⇒ 不进入 spawn 路径，无错误（静默防御）
- L2 失效：`Engine::spawn_subagent` 内 `narrowed_into` 返回 `Err(ScopeError::RecursionForbidden)` ⇒ 包装为 `EngineError::ScopeWideningRejected`（A3 §7.2 line 460）or 直接透传 ⇒ 父 agent loop 把该 tool_call 标记为 failed，落 `Item::ToolResult { error: "subagent recursion forbidden" }` ⇒ LLM 看到错误，自行调整
- L3 失效：`EngineError::RecursionForbidden { parent_tid, child_request }` ⇒ 同上落 ToolResult

> TODO(开放项 B8-4)：L3 runtime 检查若需要支持 "general-purpose 内置 subagent"（Claude Code 文档"Even without defining custom subagents, Claude can spawn the built-in `general-purpose` subagent"），zhive 是否预留 built-in subagent 注册表？倾向 **Phase 1 不做** —— 仅支持 client-defined subagents。Phase 2 再加。

---

## 5. parent → child / child → parent 数据流（关键问题 #3 + #4 完整答案）

### 5.1 parent → child：prompt 字符串 + tool 白名单（"Agent tool's prompt string is the only channel"）

| 内容 | 传递机制 | 备注 |
|---|---|---|
| **child 系统 prompt** | `SubagentDefinition.prompt` 字段；spawn 时直接 inject 到 child Thread 的 system message 槽 | wire 已定义（A3 §7.3 line 537）。client → server 在 init / settings 阶段发；server 内 `EngineConfig.subagents: HashMap<String, SubagentDefinition>` 持有 |
| **child 任务 prompt** | parent LLM 调 Agent tool 时填的 `input.prompt: String` | 这是 **parent ↔ child 唯一动态通道**（Claude Code docs 字面），含路径 / 错误 / 决策 等 |
| **child 工具白名单** | `SubagentDefinition.tools: Option<Vec<ToolName>>` + `disallowed_tools: Vec<ToolName>` | `None ⇒ 继承父全集`（A3 §7.3 line 540）。spawn 时由 `PermissionScope::narrowed_into` 校验合法（A3 §7.2 line 467-481） |
| **child permission_mode** | `SubagentDefinition.permission_mode: Option<PermissionMode>` | `None ⇒ 继承父 mode`（A3 §7.3 line 544）。narrow 检查保证不放大 |
| **child model / mcpServers / maxTurns / skills / memory / background / effort** | `SubagentDefinition` 其余字段（A3 §7.3 line 549 标注"由 B8 决"） | **B8 现在落地**：见 §5.4 表 |
| **child extension / tool / hook 可见性** | A5 `disable_in_subagent: bool`（A5-O6 是否扩展到 extension+tools+hooks） | **B8 决**：扩展（详见 §5.5）。child engine 在加载 ExtensionManifest 时跳过所有 `disable_in_subagent=true` 的 entry |

**传递机制总结**：**in-process function call**（**不**走 wire）。child engine 共享 parent engine 的 `EngineInner`，spawn 仪式发生在 `Engine::spawn_subagent` 内部，无 RPC framing 成本。这与 B6 §3.2 line 224 "subagent 不是跨进程 thread" 一致。

### 5.2 child → parent：final message（in-process channel）

| 内容 | 传递机制 | 备注 |
|---|---|---|
| **child final message** | child item appender 在落 `Item::AgentMessage { final: true }` 时同步 `subagent_final_tx.send(SubagentFinalEvent { text, child_tid, agent_id })` | parent agent loop 在 `Submission::SpawnSubagent` 的 oneshot reply 之后转入 `subagent_final_rx.recv().await` |
| **child intermediate items**（tool_call / reasoning chunk / agent_message_chunk） | **不**直接传给 parent；走 child 自己的 `ActiveTurn.item_tx` → child appender → broadcast `event_bus`（B1 §6.1） | client 端通过 `parent_tool_use_id` 字段（Claude Code docs "Detecting subagent invocation"）识别消息属于哪个 subagent context，**但 parent agent loop 不消费这些** |
| **child permission decision** | child reducer fold 出 `fold_child` ⇒ `subagent_decision_tx.send(fold_child)` ⇒ parent reducer 二次 fold（B6 §3.1） | 与 final 是**正交通道** |
| **child error / shutdown** | child agent loop 任意失败 ⇒ `subagent_final_tx.send(SubagentFinalEvent::Error(...))` | parent 把 error 落 `Item::ToolResult { error: ... }`，正常 LLM 看到错误 |

### 5.3 `SubagentFinalEvent` wire 形态（B8 现在定）

> 这是 **in-process channel payload**，不是 JSON-RPC wire。但 client 视角通过 `event_bus` 看到 `parent_tool_use_id` 标记的 messages 来识别。

```rust
// crates/zhive-core/src/engine/subagent.rs  （B8 不实现，仅给草图）

/// child → parent 的 final 回流事件（in-process channel）。
#[derive(Debug, Clone)]
pub(crate) enum SubagentFinalEvent {
    /// child 成功完成：最终一段文本（verbatim 落 parent 的 ToolResult.content）
    Completed {
        text: String,
        child_tid: ThreadId,
        /// "agent_id" 供 Claude Code "Resuming subagents" 语义用
        /// （文档 "agentId: <uuid>" 出现在 Agent tool result 内）
        agent_id: String,
    },
    /// child 失败：error 字符串
    Errored {
        error: String,
        child_tid: ThreadId,
        agent_id: String,
    },
    /// child 在等 user defer permission ⇒ parent 也需挂起 turn
    /// （B6 §7 TODO B6-O6 父子两层 suspended）
    Suspended {
        child_tid: ThreadId,
        child_request_id: String,
    },
}
```

### 5.4 child engine 接受的 SubagentDefinition 字段（B8 决，A3 §7.3 line 549 接力）

| 字段 | 处理 | 备注 |
|---|---|---|
| `name / description / prompt` | 直接 inject child system prompt | A3 已定 |
| `tools / disallowed_tools / permission_mode` | 走 `narrowed_into` 校验 + inject child PermissionScope | A3 已定 |
| `allow_subagent_spawn` | 必须 `false`（§4 三层禁递归） | A3 已定 |
| **`model`** (新增) | inject child `Engine`-time `LlmProvider` 的 model override（B10 接力，B8 仅留位 `Option<String>`） | Claude Code docs："Model override for this agent. Accepts an alias such as 'sonnet', 'opus', 'haiku', 'inherit', or a full model ID" |
| **`mcpServers`** (新增) | child engine load extension 时仅 enable 列表内的 MCP server | A5 接力 |
| **`maxTurns`** (新增) | child agent loop 在 turn count >= maxTurns 时强制 `Item::AgentMessage { final=true, reason: "max_turns" }` | 防 child 无限循环 |
| **`skills`** (新增) | child engine 加载时**只**预加载列表内的 skills；其余 skills 仍可通过 Skill tool 调用（per Claude Code docs） | A5 skill manifest 接力 |
| **`memory`** (`user / project / local`，新增) | child memory source 选择 —— Phase 1 仅支持 `project` 默认 | B2 接力 |
| **`background`** (新增) | `true` ⇒ parent **不** await child final；落 `Item::ToolResult { content: "started in background", ... }` 立即返回 | Phase 1 **不实现**（推 Phase 2），但 wire 字段保留 |
| **`effort`** (新增) | LLM reasoning effort override（per Claude Code docs `'low'/'medium'/'high'/'xhigh'/'max'/number`） | B10 接力 |

> TODO(开放项 B8-5)：`background: true` Phase 1 是否完全拒（返回 `EngineError::UnsupportedFeature`）还是退化为同步 await？倾向**拒** —— 防止 silent semantic mismatch。落地时校验。

### 5.5 A5 `disable_in_subagent` 决定（接力 A5-O6）

**决定**：扩展到 **extension 顶层 + `[[tools]]` + `[[hooks]]` 三处**。child engine 加载 ExtensionManifest 时按以下规则跳过：

| Manifest 节点 | `disable_in_subagent=true` 时的行为 |
|---|---|
| Extension 顶层 | 整个 extension（含其下所有 tools/hooks/prompts/skills）在 child engine 不加载 |
| `[[tools]]` | 该 tool 在 child engine 不注册到 tool registry —— LLM 看不到，调用即 not_found |
| `[[hooks]]` | 该 hook 在 child engine 不订阅任何事件 —— PreToolUse / PostToolUse 等都不触发 |

理由：A5-O6 提出的"是否扩展到 extension+tools+hooks"现在 D-008 父子继承场景明确需要。例如某 hook 只在 parent 做审计输出 / 一些 tool 仅 parent 可见（如 ManagePermissions），子继承时应隐藏。

---

## 6. "final" 的定义（关键问题 #4 完整答案）

### 6.1 什么是 "final"

**决定**：**`Item::AgentMessage { final: true }`**，即 A1 `Item` enum 中 `AgentMessage` variant 上的 `final: bool` 标记位。具体语义：

| Item 类别 | final 状态 | 备注 |
|---|---|---|
| `Item::AgentMessage` 普通帧 | `final=false`（默认） | 常规文本流，落 transcript 但不触发 subagent return |
| `Item::AgentMessage` 最末一帧 | **`final=true`** | child agent loop 在 `Turn` 收到 LLM 的 stop_reason ∈ {`end_turn`, `max_tokens`, `stop_sequence`} 且 LLM 响应**不含 tool_calls** 时打 final |
| `Item::ToolCall` | n/a | 不是 final candidate（intermediate） |
| `Item::ToolResult` | n/a | 不是 final candidate |
| `Item::Reasoning` | n/a | 不是 final candidate（intermediate per Claude Code docs "Intermediate tool calls and results stay inside the subagent"） |
| `Item::ContextCompaction` | n/a | 不是 final candidate |

> TODO(开放项 B8-6)：A1 deliverable §6 `Item::AgentMessage` 是否已经有 `final: bool` 字段？需要回查 A1 落地确认。若没有，B8 这里要请求 A1 加字段。当前推测**有**（与 A1 `TurnStatus = Completed/Failed/Interrupted` 对应 final flag 是标配）。落地时由 A1 落地者 + B1 落地者协调。

### 6.2 谁判定 final

**child agent loop 自己判定** —— 不需要 parent 介入：

```text
child agent loop tick:
  1. LLM stream 完成
  2. 检查最末 Item:
     - 若 ToolCall（含 tool_use blocks） ⇒ dispatch tool ⇒ loop back
     - 若 AgentMessage 且 LLM stop_reason ∈ {end_turn, max_tokens, stop_sequence} 且无 ToolCall ⇒ 标 final=true
  3. 若 final=true:
     - item_tx.send(Item::AgentMessage { final: true, text })
     - subagent_final_tx.send(SubagentFinalEvent::Completed { text, child_tid, agent_id })
     - child Turn → TurnStatus::Completed
     - child phase → Idle
```

### 6.3 多 final 怎么办

**理论上不会有**：每个 child Turn 仅一帧 final（按 §6.2 规则，stop_reason 出现一次）。但防御性设计：

| 异常 | 处理 |
|---|---|
| child agent loop bug 导致多帧 final | parent 侧 `subagent_final_rx` **仅取首帧**（`recv().await` 一次），后续帧由 channel 自然 drop（mpsc 的 receiver dropped ⇒ sender send 失败 ⇒ child 端打 warn log） |
| child Turn 在 final 之后还有 trailing item（如 `turn/completed` notification 本身） | trailing item 不走 `subagent_final_tx`，走 `event_bus` 正常 fan-out 出去（被 client 端通过 `parent_tool_use_id` 识别） |
| child agent loop 失败（panic / error）但未发 final | child loop 的 `Drop` impl 触发 `subagent_final_tx.send(SubagentFinalEvent::Errored { error: "child loop panicked", ... })` —— parent 不会永挂 |

### 6.4 final 与 child Turn / Thread 生命周期

- final 触发后 child Turn 结束 ⇒ `child.active_turn = None` ⇒ child phase = Idle
- child ThreadHandle **保留在 threads HashMap**（不立即 drop）—— 支持 Claude Code "Resuming subagents" 语义：parent 再次调 Agent tool with same agent_id 可续命
- Engine shutdown 时统一回收所有 thread（含 child）

---

## 7. 与 B6 父子继承的对接点

| 对接维度 | B6 决定 | B8 落地点 |
|---|---|---|
| `subagent_decision_tx: mpsc::Sender<PermissionDecision>` 注入时机 | B6 §3.2 line 224 "spawn 时由 parent 注入" | B8 §2.3 `ThreadHandle.subagent_decision_tx: Option<...>` 字段定义；spawn 仪式（§3 流程图 "5. spawn child agent loop task" 步骤）在 `EngineInner.threads.insert` 之前完成 sender / receiver 配对 |
| `SubagentSpawn` phase 切换 | B6 §3.2 line 227 "毫秒级仪式，不是 child 运行时态" | B8 §3 流程图明确：spawn 进入后立即切回 `Turn`（child 后续运行期间 parent 仍 `Turn`） |
| BypassPermissions short-circuit | B6 §3.3 "child hooks 仍 dispatch，返回值替换为 Allow" | B8 不重复定义；child engine 加载 hook_host 时若检测到 `permission_mode == BypassPermissions` ⇒ hook_host 中包一层 `decorator` 把所有 decision 替换为 `Allow`。该 decorator 仅对 child 启用，parent 不影响 |
| child Defer ⇒ 父子两层 suspended | B6 §7 TODO B6-O6 | B8 落地：`SubagentFinalEvent::Suspended { child_tid, child_request_id }` —— child reducer 解出 Defer ⇒ child agent loop 把该事件发给 parent ⇒ parent agent loop 解 `subagent_final_rx.recv()` 收到 Suspended ⇒ parent 也进 suspended（不发 final 给 LLM，等 client `session/resume_permission` 续命） |
| `inherited_permissions` wire 字段 | A3 §7.3 line 553 "不存在该字段" | B8 不引入；child PermissionScope 在 spawn 仪式内由 server 计算（`narrowed_into` 反向构造） |

---

## 8. 关键问题逐条作答（验收）

| # | 问题 | 答案（≤ 8 行） |
|---|---|---|
| 1 | Subagent 是新 Engine instance 还是同 Engine 内新 Thread？ | **同 Engine 内新 ThreadHandle**（共享 `EngineInner.threads`）。"fresh context window" = 新 `ThreadHandle` + 新空 `Thread.items` + 新 `ActiveTurn`。新 Engine 实例代价 ≥ 10× 新 ThreadHandle（要复制 7 个 `Arc<dyn ...>` 注入 + 自建 reverse-RPC 路由器），且 storage / hook_host / provider 实际仍要共享 parent sink ⇒ 假独立。详见 §2.1 备选表。 |
| 2 | 禁递归：spawn 入口拦 vs schema 层禁？ | **三层并行**（深度防御）：L1 schema `allow_subagent_spawn: bool` 默认 `false`（serde default）；L2 `narrowed_into` 检查 `child.allow_subagent_spawn` 返回 `ScopeError::RecursionForbidden`（A3 §7.2 line 498）；L3 `spawn_subagent` 入口 runtime 看 `parent_thread_id.is_some()` ⇒ `EngineError::RecursionForbidden`。"前者简单"（L1 零代码）"后者更严"（L3 无法静态绕过）。详见 §4.1 + §4.2。 |
| 3 | parent → child 怎么传 prompt / 工具白名单？ | **in-process function call**（不走 wire）。`SubagentDefinition`（client 在 init 时发，A3 §7.3 已定 wire）持 prompt / tools / disallowed_tools / permission_mode 等；spawn 仪式发生在 `Engine::spawn_subagent` 内部，server-side 把字段 inject 到 child `ThreadHandle.thread` + child `PermissionScope`，过程无 RPC framing。Agent tool 的 `input.prompt` 是 parent ↔ child 唯一动态通道（per Claude Code docs）。详见 §5.1。 |
| 4 | child → parent 的 final message 怎么回？什么是 "final"？ | **`Item::AgentMessage { final: true }`**：child agent loop 在 LLM stop_reason ∈ {end_turn, max_tokens, stop_sequence} 且响应**无 tool_calls** 时打标记，**自己判定**。回流通道：**in-process** `mpsc::Sender<SubagentFinalEvent>`（`subagent_final_tx`，spawn 时配对的 sender/receiver）。parent 在 `subagent_final_rx.recv()` 收第一帧 final，落 `Item::ToolResult` verbatim 给 parent LLM。多 final 防御：仅取首帧，后续 drop（mpsc 自然 fail）。详见 §6。 |

---

## 9. 未决项

> TODO(开放项 B8-1)：`subagent_decision_tx`（permission） 与 `subagent_final_tx`（final + suspended）是否合并为单一 `subagent_event_tx: mpsc::Sender<SubagentInboundEvent>` enum？倾向**保留分离**（生命周期不同：decision 每 tool_call 一次，final 仅一次）。

> TODO(开放项 B8-2)：parent 是否能对 in-flight child 发 steer / followUp？语义：`Engine::steer(parent_tid, parent_turn_id, input)` 是否能"穿透"到 child？倾向 **不能 Phase 1**：steer 仅作用于 parent ActiveTurn 的 LLM stream（即 parent 等 subagent_final_rx 的那段 wait），child Turn 内部 LLM stream 不被 parent steer 影响。若 user 要中断 child ⇒ 用 `interrupt_turn(child_tid, ...)` 单独发。

> TODO(开放项 B8-3)：child Turn 完成后 child ThreadHandle 是否立即 drop？倾向 **保留**（per Claude Code "Resuming subagents" + "Subagent transcripts persist within their session"）。但 drop 时机 / GC 策略（cleanupPeriodDays 默认 30 天）由 B3 storage / B2 memory 决定。

> TODO(开放项 B8-4)：是否预留 Claude Code "built-in general-purpose subagent" 注册位？倾向 **Phase 1 不做**，仅支持 client-defined。Phase 2 再加。

> TODO(开放项 B8-5)：`SubagentDefinition.background: true` Phase 1 处理：拒（`EngineError::UnsupportedFeature`）还是退化同步？倾向**拒**（防 silent semantic mismatch）。

> TODO(开放项 B8-6)：A1 `Item::AgentMessage` 是否已有 `final: bool` 字段？需 A1 落地者确认。本 deliverable 假设有。若没，B8 这里要请 A1 加字段。

> TODO(开放项 B8-7)：child engine 的 hook_host —— 是 parent 共享 + decorator 过滤，还是独立实例？§7 写"共享 hook_host + decorator"，但 hook host 内部状态（如 PreCompact 已发标记）是否需要 per-thread 隔离？由 B5 hook host 落地者确认。

> TODO(开放项 B8-8)：`SubagentDefinition.maxTurns` 触发时（child 达到上限）是否计入 child 失败还是优雅 final？倾向**优雅 final**（per `Item::AgentMessage { final: true, reason: "max_turns" }`，让 parent 仍能继续）。但若 parent LLM 期望具体内容 ⇒ 失败更合适。具体语义待 LLM 行为测试后定。

> TODO(开放项 B8-9)：`agent_id` 形态（Claude Code docs 文本里出现"agentId: <uuid>"）—— zhive 用 ThreadId 复用还是独立 uuid？倾向 **复用 ThreadId**（直接 `child_tid.to_string()`），减少新 id 类型。但 wire 上需在 ToolResult content 内 embed 该 id 字符串供 client extract（与 Claude Code 行为对齐）。

---

## 10. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中行号引用）
- [x] 不动 `crates/` 源码（草图均在本 markdown 内）
- [x] 不改 `research/99-decisions/`（仅引用，未编辑）
- [x] 不 `git pull`
- [x] Subagent spawn 流程图（§3 ASCII，含 parent Engine → SubagentSpawn phase → child ThreadHandle 创建 → child run_loop → final → 回流 parent）
- [x] 新 Engine vs 新 Thread 选型决定（§2.1 表 + §2.2 衔接 B1 actor pattern 代价分析）
- [x] 禁递归实现位置（§4 三层 L1/L2/L3 + 报错语义）
- [x] 与 B6 父子继承对接点（§7 表）
- [x] "final" 定义（§6.1 `Item::AgentMessage { final: true }` + §6.2 谁判定 + §6.3 多 final 怎么办）
- [x] 关键问题逐条作答（§8 表 4 题）
- [x] 未决项 9 条（TODO B8-1 ~ B8-9）

— B8 deliverable end —
