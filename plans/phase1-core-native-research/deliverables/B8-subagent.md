---
task: B8
title: Subagent 调度（fresh window / only final / 禁递归）
date: 2026-05-28
status: implemented
depends_on:
  - B1 deliverable (Engine actor pattern + EnginePhase 5 态 + channel 拓扑；subagent spawn 不占用独立 phase，作为 Turn 内 `agent` tool 运行)
  - B6 deliverable (父子 reducer 双调 + `subagent_decision_tx: mpsc::Sender<PermissionDecision>` in-process 传值 + BypassPermissions short-circuit)
  - A3 deliverable (`SubagentDefinition` wire 形态 + `narrowed_into` + `allow_subagent_spawn: bool` 默认 false)
  - A5 deliverable (`disable_in_subagent` 字段)
  - D-008 (Subagent permission inheritance / reverse-RPC / fresh context window)
references:
  - https://code.claude.com/docs/en/agent-sdk/subagents              ("Each subagent runs in its own fresh conversation. Intermediate tool calls and results stay inside the subagent; only its final message returns to the parent." + "Subagents cannot spawn their own subagents. Don't include `Agent` in a subagent's `tools` array." + AgentDefinition 字段表 + "The parent receives the subagent's final message verbatim as the Agent tool result")
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §2.1 (74-97) `EnginePhase` 5 态定义（subagent spawn 不占独立 phase）
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §2.3 (148-155) phase 转换矩阵（child 不参与全局 phase）
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (245-274) `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `Engine: Clone` actor handle
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (293-304) `ActiveTurn { item_tx: mpsc::Sender<Item> }` 单 producer 单 consumer
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md  §4 (332-352) `Submission::SpawnSubagent { parent_thread_id, definition }`（actor-dispatch 入口；`agent` tool 直接调 `spawn_subagent_awaitable` 拿 final receiver）
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
| zhive B1 `EnginePhase` 5 态（subagent spawn 不占独立 phase） | `plans/phase1-core-native-research/deliverables/B1-engine-loop.md` | §2.1 (74-97) |
| zhive B1 phase 转换矩阵（child 不参与全局 phase，parent spawn 期间仍 `Turn`） | 同上 | §2.3 (148-155) |
| zhive B1 `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `Engine: Clone`（actor pattern） | 同上 | §4 (245-274) |
| zhive B1 `Submission::SpawnSubagent { parent_thread_id, definition }`（actor-dispatch；`agent` tool 走 `spawn_subagent_awaitable`） | 同上 | §4 (348-348) |
| zhive B1 `EngineInner::spawn_subagent(parent_thread_id, definition) -> Result<ThreadId, SubagentSpawnError>` | 同上 | §4 (450-457) |
| zhive B1 `TurnKind = Regular \| Subagent \| Review`（Subagent kind 已留位） | 同上 | §4 (306-316) |
| zhive B1 `ActiveTurn.item_tx: mpsc::Sender<Item>`（单 producer / 单 consumer，turn 内事件流） | 同上 | §4 (293-304) |
| zhive B6 父子调用图（含 `parent.subagent_decision_tx.send(fold_child)` in-process channel） | `plans/phase1-core-native-research/deliverables/B6-permission-reducer.md` | §3.1 (138-217) |
| zhive B6 subagent 同 `EngineInner` 语义（child 不参与全局 phase，仅在 cancel / scope 隔离） | 同上 | §3.2 (219-227) |
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

    /// child → parent 的 permission 握手反向通道（B6 §3.2 line 224）
    /// 仅 child thread 持有；spawn 时由 parent 注入 sender，parent 持有对应 receiver
    /// 每个 child tool_call 发一条 `SubagentDecisionRequest`，parent 第二次 fold 后
    /// 经其内 `reply` oneshot 回 `ParentVerdict`（Allow / Deny）
    /// None ⇒ 顶层 thread，无父
    subagent_decision_tx: Option<mpsc::Sender<SubagentDecisionRequest>>,

    /// child → parent 的 final 回流通道
    /// **专门承载 final message**（见 §6）；child Turn 跑完后由
    /// `run_child_turn_inner` 用 `extract_final_message` 归约出 final，send 进去
    /// parent（`agent` tool）持对应 receiver，转入 `subagent_final_rx.recv().await`
    /// 每个 child Turn 仅投递一次（`Completed` 或 `Errored`）
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
   │    Err(SubagentSpawnError::RecursionForbidden)│ （§4.2）
   │ 6. spawn_subagent_awaitable(...)     │
   └──────────────────────────────────────┘
                │
                │ EnginePhase 不变（parent 全程停在 Turn）
                │ child thread 不参与全局 phase 机器
                ▼
   ┌──────────────────────────────────────┐
   │ Engine dispatcher:                   │
   │ - 创建 child ThreadId               │  ← `thread:subagent/{parent_stem}/{counter}`
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
                │ EnginePhase 始终 = Turn（spawn 全程不切 phase）
                │
                │ (parent agent loop 阻塞等 subagent_final_rx.recv())
                ▼                                                       │
   ┌──────────────────────────────────────┐                            ▼
   │ parent ActiveTurn 继续，但 LLM       │                ┌───────────────────────────┐
   │ stream 已 yield 给 Agent tool 等结果 │                │ child ThreadHandle:        │
   │ pending_tool_call.await =            │                │ - active_turn = Some(...)  │
   │   subagent_final_rx.recv()           │                │ - child 不参与全局 EnginePhase 机器 │
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
                                                                       │ Item::AgentMessage 落 item_tx（无 tool_call）
                                                                       ▼
                                                            ┌───────────────────────────┐
                                                            │ child item appender:       │
                                                            │ - 持久化（B3 JSONL）       │
                                                            │ - emit ItemAppended event  │
                                                            └───────────────────────────┘
                                                                       │
                                                                       │ child Turn 结束（run_turn 返回）⇒
                                                                       ▼
                                                            ┌───────────────────────────┐
                                                            │ run_child_turn_inner:      │
                                                            │ - extract_final_message(   │
                                                            │     transcript tail) =     │
                                                            │     最后一个 AgentMessage  │
                                                            │     （回退 SystemNotice）  │
                                                            │ - subagent_final_tx.send(  │
                                                            │     SubagentFinalEvent::   │
                                                            │       Completed {          │
                                                            │         child_thread_id,   │
                                                            │         final_message })   │
                                                            └───────────────────────────┘
                                                                       │
                ┌──────────────────────────────────────────────────────┘
                ▼
   ┌──────────────────────────────────────┐
   │ parent agent loop（`agent` tool）解 await: │
   │ subagent_final_rx.recv() = Ok(final) │
   │ - 构造 ToolResult { content: final_message 文本, parent_tool_use_id: child_tid 标记 }
   │ - 落 Item::ToolResult 进 parent ActiveTurn.item_tx
   │ - parent LLM 下一轮请求把该 ToolResult 当作 Agent tool 的返回值
   └──────────────────────────────────────┘
                │
                │ deliver_subagent_outcome 完成两路投递
                │ （in-process channel + broadcast SubagentCompleted）后：
                │ child ThreadHandle 从 threads HashMap 移除
                │ （on-disk rollout + SQL 行保留，历史仍可查）
                ▼
   ┌──────────────────────────────────────┐
   │ parent ActiveTurn 继续；child handle  │
   │ 已从内存 threads HashMap 移除          │
   │ （resume 续命语义为 Phase 2 前向计划， │
   │  见 §7 未决 B8-3）                     │
   └──────────────────────────────────────┘
```

---

## 4. 禁递归实现位置（关键问题 #2 完整答案）

### 4.1 决定：**spawn 入口拦（runtime 检查）+ schema 默认禁（serde default false）双层**

| 层 | 位置 | 检查内容 | 错误类型 |
|---|---|---|---|
| **L1 schema 默认** | `SubagentDefinition.allow_subagent_spawn: bool` 默认 `false`（A3 §7.3 line 547-548 `#[serde(default)]`） | client 构造 SubagentDefinition 时若不显式打开 ⇒ 该 subagent 无 Agent tool | 静态：tools[] 内没 `Agent` |
| **L2 scope narrow 时检查** | `PermissionScope::narrowed_into(child)` 内 `if child.allow_subagent_spawn { Err(ScopeError::RecursionForbidden) }` （A3 §7.2 line 498-501） | parent 构造 child scope 时若 child 试图开 `allow_subagent_spawn = true` ⇒ 拒 | `ScopeError::RecursionForbidden` |
| **L3 spawn 入口 runtime 拦** | `spawn_subagent` / `spawn_subagent_awaitable` 经 `prepare_child_scope` 检查 `parent_handle.parent_thread_id.is_some()`（`SubagentError::ParentIsSubagent`）⇒ 映射为 `SubagentSpawnError::RecursionForbidden` | 父 thread 自身是 child ⇒ 拒 spawn | `SubagentSpawnError::RecursionForbidden` |

### 4.2 为什么要三层（"前者简单后者更严"的折中）

- **L1 schema 默认 false**：与 Claude Code 文档 "Subagents cannot spawn their own subagents. Don't include `Agent` in a subagent's `tools` array" 字面对齐。这是**第一道防线**（用户错误配置不会导致灾难）
- **L2 narrowed_into**：A3 已把 `allow_subagent_spawn: bool` 当 narrow 检查项。这一道防 client 显式打开 `allow_subagent_spawn=true` 的情况（即使 schema 允许，子缩窄规则也拒）
- **L3 runtime 拦**：万一前两道被绕（如未来扩展 default true 或某 client 跳过 narrow check），最后一道在 spawn 入口看 `parent_thread_id.is_some()` ——**对运行时事实的检查**，无法被静态配置绕过

**"前者简单"**：L1 是 0 行代码（仅 `#[serde(default)]`）。"**后者更严**"：L3 在 runtime 强制，无法绕过。zhive 选**三层并行**（深度防御 / defense-in-depth），任一层失效不会导致递归。

### 4.3 报错语义

- L1 失效：subagent 没有 Agent tool ⇒ LLM 不会调用 Agent tool ⇒ 不进入 spawn 路径，无错误（静默防御）
- L2 失效：`prepare_child_scope` 内 `narrowed_into` 返回 `Err(ScopeError::RecursionForbidden)`（`SubagentError::ChildSpawnRequested`）⇒ 映射为 `SubagentSpawnError::ScopeWideningRejected` ⇒ 父 agent loop 把该 tool_call 标记为 failed，落 `Item::ToolResult { error: "subagent recursion forbidden" }` ⇒ LLM 看到错误，自行调整
- L3 失效：`SubagentSpawnError::RecursionForbidden` ⇒ 同上落 ToolResult

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
| **child model / mcpServers / maxTurns / skills / memory / background / effort** | Claude Code `AgentDefinition` 扩展面，当前 `SubagentDefinition` wire 未含 | **Phase 2 前向计划**：见 §5.4 表 |
| **child extension / tool / hook 可见性** | A5 `disable_in_subagent: bool`（A5-O6 是否扩展到 extension+tools+hooks） | **B8 决**：扩展（详见 §5.5）。child engine 在加载 ExtensionManifest 时跳过所有 `disable_in_subagent=true` 的 entry |

**传递机制总结**：**in-process function call**（**不**走 wire）。child engine 共享 parent engine 的 `EngineInner`，spawn 仪式发生在 `Engine::spawn_subagent` 内部，无 RPC framing 成本。这与 B6 §3.2 line 224 "subagent 不是跨进程 thread" 一致。

### 5.2 child → parent：final message（in-process channel）

| 内容 | 传递机制 | 备注 |
|---|---|---|
| **child final message** | child Turn 结束后 `run_child_turn_inner` 用 `extract_final_message` 取 transcript tail 的最后一个 `Item::AgentMessage`（回退 `SystemNotice`），`subagent_final_tx.send(SubagentFinalEvent::Completed { child_thread_id, final_message: Option<Arc<Item>> })` | parent（`agent` tool）持 final receiver，转入 `subagent_final_rx.recv().await` |
| **child intermediate items**（tool_call / reasoning chunk / agent_message_chunk） | **不**直接传给 parent；走 child 自己的 `ActiveTurn.item_tx` → child appender → broadcast `event_bus`（B1 §6.1） | client 端通过 `parent_tool_use_id` 字段（Claude Code docs "Detecting subagent invocation"）识别消息属于哪个 subagent context，**但 parent agent loop 不消费这些** |
| **child permission decision** | child reducer fold 出 `fold_child` ⇒ `subagent_decision_tx.send(fold_child)` ⇒ parent reducer 二次 fold（B6 §3.1） | 与 final 是**正交通道** |
| **child error / shutdown** | child agent loop 任意失败 ⇒ `subagent_final_tx.send(SubagentFinalEvent::Errored { child_thread_id, error: TurnError })`（透传真实 `TurnError`） | parent 把 error 落 `Item::ToolResult { error: ... }`，正常 LLM 看到错误 |

### 5.3 `SubagentFinalEvent` 形态

> 这是 **in-process channel payload**，不是 JSON-RPC wire。client 视角通过 `event_bus` 看到 `parent_tool_use_id` 标记的 messages 来识别，并消费广播的 `EngineEvent::SubagentCompleted`。

```rust
// crates/zhive-core/src/subagent.rs

/// child → parent 的 final 回流事件（in-process channel）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SubagentFinalEvent {
    /// child 成功完成：final message 是 transcript tail 最后一个
    /// `Item::AgentMessage`（回退 `SystemNotice`），verbatim 落 parent
    /// 的 ToolResult.content；transcript 二者皆无时为 None。
    Completed {
        child_thread_id: ThreadId,
        final_message: Option<Arc<Item>>,
    },
    /// child 失败：透传真实 `TurnError`（不是 sentinel 字符串）。
    Errored {
        child_thread_id: ThreadId,
        error: TurnError,
    },
    /// 预留变体：当前全 handshake 架构下不构造 —— child 的 Defer 经
    /// in-process `subagent_decision_tx` 路由到 parent 第二次 fold，由
    /// parent inline 解（Allow / Deny）后 child 才继续，故 child 不发终态
    /// Suspended。保留供未来 child 可独立挂起的架构（B6 §7 TODO B6-O6）。
    Suspended {
        child_thread_id: ThreadId,
        child_request_id: String,
    },
}
```

### 5.4 child engine 接受的 SubagentDefinition 字段

Phase 1 wire 形态（`crates/zhive-proto/src/permission.rs::SubagentDefinition`）只含以下字段：

| 字段 | 处理 | 备注 |
|---|---|---|
| `name / description / prompt` | 直接 inject child system prompt | A3 已定 |
| `tools / disallowed_tools / permission_mode` | 走 `narrowed_into` 校验 + inject child PermissionScope | A3 已定 |
| `allow_subagent_spawn` | 必须 `false`（§4 禁递归） | A3 已定 |

下列字段为 Claude Code `AgentDefinition` 的扩展面，**Phase 2 前向计划**（当前 wire 未含）：

| 字段 | 计划处理 | 备注 |
|---|---|---|
| `model` | inject child model override（B10 接力） | Claude Code docs："Model override for this agent. Accepts an alias such as 'sonnet', 'opus', 'haiku', 'inherit', or a full model ID" |
| `mcpServers` | child engine load extension 时仅 enable 列表内的 MCP server | A5 接力 |
| `maxTurns` | child agent loop 在 turn count >= maxTurns 时强制收尾返回 final | 防 child 无限循环 |
| `skills` | child engine 加载时**只**预加载列表内的 skills；其余 skills 仍可通过 Skill tool 调用（per Claude Code docs） | A5 skill manifest 接力 |
| `memory` (`user / project / local`) | child memory source 选择 —— 默认 `project` | B2 接力 |
| `background` | `true` ⇒ parent **不** await child final；立即返回 | 后台执行语义 |
| `effort` | LLM reasoning effort override（per Claude Code docs `'low'/'medium'/'high'/'xhigh'/'max'/number`） | B10 接力 |

> TODO(开放项 B8-5)：引入 `background: true` 时是否完全拒（返回 `EngineError::UnsupportedFeature`）还是退化为同步 await？倾向**拒** —— 防止 silent semantic mismatch。落地时校验。

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

**决定**：final message = child Turn 结束后，从 child transcript tail 选出的**最后一个 `Item::AgentMessage`**（无 `AgentMessage` 时回退最后一个 `Item::SystemNotice`，二者皆无则 `None`）。`Item::AgentMessage` 上**没有** `final` 标记位 —— final 不是 per-item flag，而是由 `crate::subagent::extract_final_message(transcript)` 在 turn 结束后归约得出。具体语义：

| Item 类别 | 是否 final candidate | 备注 |
|---|---|---|
| `Item::AgentMessage` | **是**（取 transcript 中最后一个） | child 完成时的文本回复，verbatim 落 parent ToolResult |
| `Item::SystemNotice` | 回退候选 | 无 `AgentMessage` 时退而取最后一个 `SystemNotice` |
| `Item::ToolCall` | 否 | intermediate |
| `Item::ToolResult` | 否 | intermediate |
| `Item::AgentThought` / `Reasoning` | 否 | intermediate（per Claude Code docs "Intermediate tool calls and results stay inside the subagent"） |
| `Item::ContextCompaction` | 否 | 不是 final candidate |

### 6.2 谁判定 final

**child turn 跑完后由 `run_child_turn_inner` 归约** —— 不需要 parent 介入，也不在 agent loop 内逐帧打标记：

```text
1. child agent loop 正常跑（含多轮 tool_call / dispatch），由 run_turn 驱动至完成
2. run_turn 返回（child active_turn 已 finish，transcript 稳定）
3. run_child_turn_inner:
   - 取 child transcript tail = child_handle.items_snapshot()
   - extract_final_message(tail) = 最后一个 AgentMessage（回退 SystemNotice）
   - deliver_subagent_outcome:
       subagent_final_tx.send(SubagentFinalEvent::Completed { child_thread_id, final_message })
       broadcast EngineEvent::SubagentCompleted { ..., final_message }
   - 从 threads HashMap 移除 child handle
```

### 6.3 投递语义

final 是 turn 结束后一次性归约（§6.2），每个 child Turn 仅投递一次 `SubagentFinalEvent`。边界处理：

| 情形 | 处理 |
|---|---|
| transcript 既无 `AgentMessage` 也无 `SystemNotice` | `extract_final_message` 返回 `None` ⇒ `Completed { final_message: None }`，parent 落空 ToolResult |
| child Turn 内 final 之后仍有 trailing item（如 `turn/completed` notification） | trailing item 不进 final 归约，走 `event_bus` 正常 fan-out（被 client 通过 `parent_tool_use_id` 识别） |
| child turn 失败（provider / stream error） | `run_turn` 返回真实 `TurnError` ⇒ `SubagentFinalEvent::Errored { child_thread_id, error }`（透传真实错误，非 sentinel）⇒ parent 不会永挂 |

### 6.4 final 与 child Turn / Thread 生命周期

- child Turn 跑完 ⇒ `run_turn` 已 `finish_turn` ⇒ `child.active_turn = None`（child 不参与全局 phase）
- `deliver_subagent_outcome` 两路投递完成后，child ThreadHandle **从 threads HashMap 移除**（避免长会话累积内存句柄）；on-disk rollout + SQL 行保留，历史仍可查
- resume 续命语义（parent 再次调 `agent` tool 续上同一 child）为 Phase 2 前向计划（§9 未决 B8-3）
- Engine shutdown 时统一回收所有 thread

---

## 7. 与 B6 父子继承的对接点

| 对接维度 | B6 决定 | B8 落地点 |
|---|---|---|
| `subagent_decision_tx: mpsc::Sender<SubagentDecisionRequest>` 注入时机 | B6 §3.2 line 224 "spawn 时由 parent 注入" | B8 §2.3 `ThreadHandle.subagent_decision_tx: Option<...>` 字段定义；`ThreadHandle::new_child` 创建 channel 并返回 receiver，在 `EngineInner.threads.insert` 之前完成 sender / receiver 配对 |
| 全局 phase 不受 spawn 影响 | B6 §3.2 line 227：subagent 不是独立运行时态 | B8 §3 流程图明确：spawn 全程不切 phase；parent 一直停在 `Turn`，child thread 不参与全局 phase 机器 |
| BypassPermissions short-circuit | B6 §3.3 "child hooks 仍 dispatch，返回值替换为 Allow" | B8 不重复定义；child engine 加载 hook_host 时若检测到 `permission_mode == BypassPermissions` ⇒ hook_host 中包一层 `decorator` 把所有 decision 替换为 `Allow`。该 decorator 仅对 child 启用，parent 不影响 |
| child Defer ⇒ 父子两层 suspended | B6 §7 TODO B6-O6 | 当前全 handshake 架构下 child 不发终态 suspended：child 的 Defer 经 `subagent_decision_tx` 路由到 parent 第二次 fold，由 parent inline 解（Allow / Deny）后 child 才继续。`SubagentFinalEvent::Suspended { child_thread_id, child_request_id }` 变体已保留，供未来 child 可独立挂起的架构（spawner 已有转发路径，命中时打 warn） |
| `inherited_permissions` wire 字段 | A3 §7.3 line 553 "不存在该字段" | B8 不引入；child PermissionScope 在 spawn 仪式内由 server 计算（`narrowed_into` 反向构造） |

---

## 8. 关键问题逐条作答（验收）

| # | 问题 | 答案（≤ 8 行） |
|---|---|---|
| 1 | Subagent 是新 Engine instance 还是同 Engine 内新 Thread？ | **同 Engine 内新 ThreadHandle**（共享 `EngineInner.threads`）。"fresh context window" = 新 `ThreadHandle` + 新空 `Thread.items` + 新 `ActiveTurn`。新 Engine 实例代价 ≥ 10× 新 ThreadHandle（要复制 7 个 `Arc<dyn ...>` 注入 + 自建 reverse-RPC 路由器），且 storage / hook_host / provider 实际仍要共享 parent sink ⇒ 假独立。详见 §2.1 备选表。 |
| 2 | 禁递归：spawn 入口拦 vs schema 层禁？ | **三层并行**（深度防御）：L1 schema `allow_subagent_spawn: bool` 默认 `false`（serde default）；L2 `narrowed_into` 检查 `child.allow_subagent_spawn` 返回 `ScopeError::RecursionForbidden`（A3 §7.2 line 498）；L3 `spawn_subagent` 入口 runtime（经 `prepare_child_scope`）看 `parent_thread_id.is_some()` ⇒ `SubagentSpawnError::RecursionForbidden`。"前者简单"（L1 零代码）"后者更严"（L3 无法静态绕过）。详见 §4.1 + §4.2。 |
| 3 | parent → child 怎么传 prompt / 工具白名单？ | **in-process function call**（不走 wire）。`SubagentDefinition`（client 在 init 时发，A3 §7.3 已定 wire）持 prompt / tools / disallowed_tools / permission_mode 等；spawn 仪式发生在 `Engine::spawn_subagent` 内部，server-side 把字段 inject 到 child `ThreadHandle.thread` + child `PermissionScope`，过程无 RPC framing。Agent tool 的 `input.prompt` 是 parent ↔ child 唯一动态通道（per Claude Code docs）。详见 §5.1。 |
| 4 | child → parent 的 final message 怎么回？什么是 "final"？ | final = child Turn 结束后由 `extract_final_message` 从 transcript tail 选出的**最后一个 `Item::AgentMessage`**（回退最后一个 `SystemNotice`，皆无则 `None`）；`AgentMessage` 上**无** `final` flag。回流通道：**in-process** `mpsc::Sender<SubagentFinalEvent>`（`subagent_final_tx`，spawn 时配对）+ broadcast `EngineEvent::SubagentCompleted`。parent（`agent` tool）`subagent_final_rx.recv()` 收 `Completed { final_message }`，落 `Item::ToolResult` verbatim 给 parent LLM；失败则 `Errored { error: TurnError }`。详见 §6。 |

---

## 9. 未决项

> TODO(开放项 B8-1)：`subagent_decision_tx`（permission） 与 `subagent_final_tx`（final + suspended）是否合并为单一 `subagent_event_tx: mpsc::Sender<SubagentInboundEvent>` enum？倾向**保留分离**（生命周期不同：decision 每 tool_call 一次，final 仅一次）。

> TODO(开放项 B8-2)：parent 是否能对 in-flight child 发 steer / followUp？语义：`Engine::steer(parent_tid, parent_turn_id, input)` 是否能"穿透"到 child？倾向 **不能 Phase 1**：steer 仅作用于 parent ActiveTurn 的 LLM stream（即 parent 等 subagent_final_rx 的那段 wait），child Turn 内部 LLM stream 不被 parent steer 影响。若 user 要中断 child ⇒ 用 `interrupt_turn(child_tid, ...)` 单独发。

> TODO(开放项 B8-3)：child Turn 完成后内存中的 child ThreadHandle 在 `deliver_subagent_outcome` 末尾即从 threads HashMap 移除（on-disk rollout + SQL 行保留）。Resume 续命（per Claude Code "Resuming subagents"：parent 再次调 `agent` tool 续上同一 child）为 Phase 2 前向计划，届时 drop 时机 / GC 策略（cleanupPeriodDays 默认 30 天）由 B3 storage / B2 memory 决定。

> TODO(开放项 B8-4)：是否预留 Claude Code "built-in general-purpose subagent" 注册位？倾向 **Phase 1 不做**，仅支持 client-defined。Phase 2 再加。

> TODO(开放项 B8-5)：`SubagentDefinition.background: true` Phase 1 处理：拒（`EngineError::UnsupportedFeature`）还是退化同步？倾向**拒**（防 silent semantic mismatch）。

> TODO(开放项 B8-7)：child engine 的 hook_host —— 是 parent 共享 + decorator 过滤，还是独立实例？§7 写"共享 hook_host + decorator"，但 hook host 内部状态（如 PreCompact 已发标记）是否需要 per-thread 隔离？由 B5 hook host 落地者确认。

> TODO(开放项 B8-8)：引入 `maxTurns` 后，child 达到上限时是否计入 child 失败还是优雅收尾返回 final message？倾向**优雅 final**（让 parent 仍能继续）。但若 parent LLM 期望具体内容 ⇒ 失败更合适。具体语义待 LLM 行为测试后定。

> TODO(开放项 B8-9)：`agent_id` 形态（Claude Code docs 文本里出现"agentId: <uuid>"）—— zhive 用 ThreadId 复用还是独立 uuid？倾向 **复用 ThreadId**（直接 `child_tid.to_string()`），减少新 id 类型。但 wire 上需在 ToolResult content 内 embed 该 id 字符串供 client extract（与 Claude Code 行为对齐）。

---

## 10. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中行号引用）
- [x] 不动 `crates/` 源码（草图均在本 markdown 内）
- [x] 不改 `research/99-decisions/`（仅引用，未编辑）
- [x] 不 `git pull`
- [x] Subagent spawn 流程图（§3 ASCII，含 parent Engine（phase 不变）→ child ThreadHandle 创建 → child run_loop → final 归约 → 回流 parent → child handle 移除）
- [x] 新 Engine vs 新 Thread 选型决定（§2.1 表 + §2.2 衔接 B1 actor pattern 代价分析）
- [x] 禁递归实现位置（§4 三层 L1/L2/L3 + 报错语义）
- [x] 与 B6 父子继承对接点（§7 表）
- [x] "final" 定义（§6.1 transcript tail 最后一个 `Item::AgentMessage`（回退 `SystemNotice`），由 `extract_final_message` 归约 + §6.2 谁判定 + §6.3 投递语义）
- [x] 关键问题逐条作答（§8 表 4 题）
- [x] 未决项（前向 / Phase 2 开放问题，见 §9）

— B8 deliverable end —
