---
plan: phase1-core-native-research
date: 2026-05-28
status: implemented
scope: Phase 1 / 第一步
crates: zhive-proto, zhive-core, zhive-client-native
depends:
  - research/99-decisions D-001~D-015
  - research/92-reference-mapping
  - research/91-architecture-review-2026-05-27
---

# Phase 1 第一步 — core 架构 + native-client 实现调研计划

## 0. 用法

本计划**不是**实现 todo list，而是 **"开写之前必须读完哪些代码 / 回答完哪些问题 / 产出哪些设计 artifact"** 的工作清单。任务分块独立，可并行分发给 subagent。每个任务必须先产出 deliverable 才能进入对应模块的编码。

参考代码本地根目录：`~/Desktop/code/github/`

| 缩写 | 仓库路径 | 用途锚点 |
|---|---|---|
| `${CODEX}` | `~/Desktop/code/github/codex/codex-rs` | 协议 + transport + 持久化（多库结构）+ client 架构 全套蓝本 |
| `${ACP}`   | `~/Desktop/code/github/acp-rust-sdk`     | ACP 0.12.1 schema + session lifecycle |
| `${RMCP}`  | `~/Desktop/code/github/rmcp`             | MCP 1.7 tool + content schema |
| `${LSP}`   | `~/Desktop/code/github/tower-lsp`        | stdio JSON-RPC 事件循环 / 取消 标杆 |
| `${SQL}`   | `~/Desktop/code/github/rusqlite`         | rusqlite 0.40 API + bundled feature |
| `${PI}`    | `~/Desktop/code/github/pi`               | TS 单体仓库（agent / ai / coding-agent / tui 四 package）；steer/followUp/nextTurn 三队列 + EnginePhase + JSONL+Leaf 指针 + extension manifest |

> 锚点 commit 在 [§ 8 调研基线](#8-调研基线) 记录，避免 upstream 漂移导致路径失效。

---

## 1. 目标 / 非目标

### 1.1 目标

- 把 **zhive-core 引擎骨架**（engine / state / persistence / server / hooks / permission / cancel / tracing / LLM provider 占位）从决策落到**可编码的模块草图**
- 把 **zhive-client-native** 的 API surface / 重连 / 取消 / 反向 RPC handler 注册机制设计稳
- 把 **zhive-proto** 已就位的 JSON-RPC envelope + framing 之外**缺失的 schema**（三层原语 / initialize / Hook / Permission / Capabilities）补完字段命名与 enum case
- 所有设计选择**必有参考点**（cloned repo 路径 + 文件 + 行号 / 字段名），不存在"凭感觉"
- 调研结束后，能在不重读 5 个 repo 的情况下**直接开 PR 实现**

### 1.2 非目标（不要做）

- ❌ 写任何 zhive crate 的实现代码（除非是为验证设计假设的最小 prototype，且写完即移到 deliverable 目录）
- ❌ TUI、CLI、bridge-stdio 三块（属于 Phase 1 但不在本调研范围）
- ❌ rmcp / ACP runtime 在 core 内的接入（D-005 明确只在 bridge crate 引用；本调研只看 schema 字段命名）
- ❌ 任何 Phase 2 / Phase 3 内容（remote / Web / A2A）
- ❌ 新增 dependency 评估（除非命中 [§ 9 开放项](#9-风险与开放项)，否则按 CLAUDE.md 红线 1 拒绝）

---

## 2. 现状盘点

zhive-proto 已就位（不重复研究）：

- ✅ `Message / Request / Response / Notification / Id / ErrorObject` JSON-RPC 2.0 envelope
- ✅ `Version` phantom marker（强制 `"2.0"`）
- ✅ `framing` module 占位（LSP Content-Length 风格）

zhive-core 已就位（仅 lib.rs / engine.rs / state.rs 三个文件，内容为骨架）：

- 待补：state 内的 Thread/Turn/Item domain 类型 + 状态机
- 待补：engine 内的 agent loop
- **完全空白**：persistence / server / hooks / permission / cancel / tracing / provider

zhive-client-native 完全空白（除 `version()`）。

本计划聚焦"待补 + 空白"部分。

---

## 3. 调研块拆分

三大 block，**Block A 提供 schema**，Block B/C 消费 Block A 的产出。Block B 与 Block C 间可并行（client-native 只依赖 proto，不依赖 core）。

```
          ┌──────────────────────────┐
          │  Block A: zhive-proto    │
          │  schema 字段命名 + enum  │
          └──────────────┬───────────┘
                         │
            ┌────────────┴────────────┐
            │                         │
            ▼                         ▼
  ┌──────────────────┐      ┌────────────────────────┐
  │  Block B: core   │      │  Block C: client-native│
  │  (B1~B10)        │      │  (C1~C4)               │
  └──────────────────┘      └────────────────────────┘
```

---

## 4. Block A — zhive-proto schema 补完

> 当前 proto 只有 JSON-RPC 信封。本块负责把 D-006/D-007/D-008/D-012 落到具体 Rust 类型 + 字段名。

### A1 · 三层原语 Thread/Turn/Item domain schema

**目的**：把 D-006 决策落到 Rust enum + struct，schemars 出 JSON Schema。

**参考点**：

- `${CODEX}/app-server-protocol/src/v2/` 找 `thread / turn / item` 三类型的字段定义
- `${ACP}/rust/src/lib.rs` 或 `${ACP}/rust/src/schema.rs` 看 `SessionUpdate` 的 10 个 case
- `${RMCP}/crates/rmcp/src/model.rs` 看 `RawContent` / `ContentBlock` 5 case 命名

**关键问题**：

1. `Item` enum 的全集 case：`reasoning / tool_call / exec / file_edit / agent_message / diff / terminal / thought` —— 字段名对齐谁？
2. `Thread.id ↔ ACP.SessionId` 的 namespace 设计（D-006 "桥接表 + ID 命名空间"具体怎么编码？）
3. `Turn` 在 MCP 侧没有等价物，bridge 侧合成 Turn 边界 —— core 这边怎么暴露 "Turn 开始 / 结束" 给 bridge？

**Deliverable**：`plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md`

- Rust 类型草图（带 `#[derive(Serialize, Deserialize, JsonSchema)]`、`#[serde(tag = "kind")]` 选择）
- 字段命名表：`zhive 字段 | 对齐源 | 备注`
- ACP `SessionUpdate` 10 case → zhive Item 映射表
- MCP `RawContent` 5 case → zhive Item 映射表

### A2 · initialize 握手 + capabilities 协商

**目的**：D-007 落地。

**参考点**：

- `${CODEX}/app-server-protocol/src/v1/` 与 `src/v2/` 找 `Initialize` 双命名空间共存的具体做法
- `${ACP}/rust/src/lib.rs` 找 `Initialize` 字段 `protocolVersion / clientCapabilities / serverCapabilities`
- ACP spec：<https://agentclientprotocol.com/protocol/schema#initialize>

**关键问题**：

1. `protocolVersion` 用 semver 还是整数？两端协商失败的错误码？
2. `capabilities` 独立 flag 怎么编码？`{ hooks: bool, subagents: bool, ... }` vs `{ hooks: { version: "1" }, ... }`（前者简单，后者可扩展，选哪个）
3. v1/v2 method 命名前缀路径（codex 选了 `v1/...` 还是其他？）

**Deliverable**：`deliverables/A2-initialize-capabilities.md`

- `Initialize{Request,Response}` 类型草图
- `Capabilities` struct 字段表（6 个 flag）
- 与 ACP `Initialize` 字段对齐 / 不对齐的逐项说明
- v1/v2 命名空间在 method 字符串里的编码规则

### A3 · Permission schema + StreamingBehavior + Subagent 继承

**目的**：D-008 落地，含 R5 finding #1 提的 `StreamingBehavior` 取消状态机。**含 Pi 三队列模型修订**。

**参考点**：

- Claude Code Hooks docs：<https://code.claude.com/docs/en/agent-sdk/hooks>（字段命名硬约束）
- Claude Code Subagents docs：<https://code.claude.com/docs/en/agent-sdk/subagents>
- `${PI}/packages/agent/src/harness/agent-harness.ts:183-186` —— `steerQueue / followUpQueue / nextTurnQueue` 三队列声明
- `${PI}/packages/agent/src/types.ts:44` —— `QueueMode = "all" | "one-at-a-time"` 定义（A3 调研验证：`harness/types.ts` 仅 import；定义在上一级 `src/types.ts:44`）
- `${PI}/packages/agent/src/agent-loop.ts:167,174-190,253-261` —— turn 内 / turn 后 注入时序
- `${PI}/packages/agent/src/harness/agent-harness.ts:391-401` —— `drainQueuedMessages` 失败 unshift 回滚
- `${PI}/packages/agent/src/harness/agent-harness.ts:936-963` —— `abort()` 清队列 + 发 `clearedSteer/clearedFollowUp` 事件（nextTurnQueue 不清）
- `${PI}/packages/coding-agent/src/modes/rpc/rpc-types.ts:21` —— wire field `streamingBehavior?: "steer" | "followUp"`
- `${ACP}/rust/src/lib.rs` 找 `permission/request` reverse RPC 形状

**关键问题**：

1. `PermissionDecision` 四态 enum 序列化形式（`"deny" / "defer" / "ask" / "allow"`）
2. Reducer 合并函数签名：`fn reduce(decisions: &[PermissionDecision]) -> PermissionDecision` —— 给 core 用 in-process trait 还是单纯 fn？
3. **三队列模型**（取代 D-008 原二元）：
   - `Steer`：turn 执行期间注入，对下一个 LLM 请求立即可见
   - `FollowUp`：agent 无更多 action 时注入
   - `NextTurn`：在 abort 时**不清空**，是恢复 / 重发关键
   - 每队列独立 `QueueMode { All, OneAtATime }`
4. `StreamingBehavior::Steer` 触发时：
   - in-flight tool_call 是否撤销？（Pi 不撤：steer 注入消息但当前 turn 工具继续执行，下一轮 LLM 请求才看到 steer 消息）
   - 已发的 reverse-request 怎么回收？
   - Turn 边界是否重置？
5. `Subagent.inherited_permissions` 字段在 wire 上长什么样？
6. Schema 字段命名是否完全对齐 Claude Code（`hookSpecificOutput.permissionDecision` etc）？
7. `abort()` 语义：清 steer/followUp，**保留 nextTurn**，发 `{ type: "abort", clearedSteer, clearedFollowUp }` 通知 extension（zhive 是否完全对齐？）

**Deliverable**：`deliverables/A3-permission-streaming-subagent.md`

- `PermissionDecision / PermissionScope / StreamingBehavior` enum 草图
- **三队列 + QueueMode** 类型与状态机
- `HookSpecificOutput` struct 与 Claude Code 逐字段对齐表
- StreamingBehavior 取消状态机 ASCII 时序图（覆盖 in-flight tool_call / reverse-request / Turn 边界 三条线 + 三队列 lifecycle）
- Subagent 继承规则的不变式（父→子单向、子可缩窄不可放大、reducer 父子各执行一次）
- `abort` 事件 wire schema + nextTurn 保留语义说明

### A4 · Hook event schema（14+ 事件）

**目的**：D-012 落地 + **Pi 24+ event 校验下界**。

**参考点**：

- Claude Code Hooks docs 19 事件全集
- D-012 已锁定的 14 + 5 reserved 清单
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:950-972` —— `ExtensionEvent` ~24 case union
- `${PI}/packages/agent/src/harness/types.ts:618-639` —— `AgentHarnessOwnEvent` 17 case（harness 自身）
- `${PI}/packages/agent/src/harness/types.ts:485` —— `AgentHarnessPhase` 5 态枚举（idle/turn/compaction/branch_summary/retry）

**关键问题**：

1. `HookEvent` enum 是否真的能 `#[non_exhaustive]` + 反序列化未知 case 优雅降级？写一个最小 prototype 验证
2. Hook input 的 base 字段（`session_id / cwd / hook_event_name`）放 wrapper struct 还是各 event 自带？
3. Subagent 上下文字段（`agent_id / agent_type / parent_tool_use_id`）放哪里？
4. **base 字段必含 `registered_by: ExtensionRef`**（红线 10）—— wire 字段名 / 编码？
5. Pi 把 hooks 分成"harness-level"（17 个 core 关切：message/tool/provider/session）与"extension-level"（24 个应用关切：resources_discover/input/bash 等）—— zhive 14 个事件是否要做类似分层？分层意味着 wire schema 加 `category` 字段
6. Pi 比 Claude Code 多出来的"compaction / branch_summary / leaf"相关事件 —— zhive 14 reserved 是否需要补这几个？

**Deliverable**：`deliverables/A4-hook-event-schema.md`

- `HookEvent` enum 14 case + `#[non_exhaustive]` 完整 Rust 草图
- `HookEventBase { session_id, cwd, hook_event_name, registered_by, agent_id?, agent_type?, parent_tool_use_id? }`
- Hook input JSON 示例 × 14（每个 event 给一份 fixture）
- 反序列化未知 case 的策略（保留 raw JSON 还是降级到 `Unknown { name, payload }`）
- 与 Pi 24 + 17 事件的逐项对照表（标 ✅ 覆盖 / ⚠️ 缺 / ❌ 拒）

### A5 · Extension manifest schema（D-013 落地）

**目的**：把 manifest 字段定下来，避免 Phase 2 改 wire。**Pi 仓库已可读，R5 finding #2 不再"待补"**。

**参考点**：

- `${PI}/packages/coding-agent/src/core/extensions/types.ts:426-472` —— `ToolDefinition` 12 字段（`name / label / description / promptSnippet / promptGuidelines / parameters / renderShell / prepareArguments / executionMode / execute / renderCall / renderResult`）
- `${PI}/packages/coding-agent/src/modes/rpc/rpc-types.ts:76-85` —— `RpcSlashCommand { name, description, source: "extension"|"prompt"|"skill", sourceInfo }`
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:494-506` —— `ResourcesDiscoverEvent` + `ResourcesDiscoverResult { skillPaths?, promptPaths?, themePaths? }`：extension 动态贡献资源路径
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:551-557` —— `session_shutdown` + `invalidate()` 生命周期（警告：Pi 没完全解决 zombie listener）
- Gemini CLI extensions 文档（如有 clone）

**关键问题**：

1. R5 finding #2 字段全集回答：抄 Pi `ToolDefinition` 12 字段，逐项决定 zhive 是否保留 / 改名 / 拒收
2. Hook 通过 manifest 注册 vs settings 顶层注册的优先级 / 覆盖规则？
3. `ResourcesDiscoverEvent` 动态贡献路径机制是否进 Phase 1？（Pi 用此让 extension 注册自定义 skill / prompt 目录）
4. `renderCall / renderResult` Component 在 zhive 怎么对应？（Pi 是 React 组件；zhive TUI 是 ratatui，schema 层只能存 JSON 描述符）
5. Extension 热重载（`invalidate()`）的 zombie listener 风险（Pi 反例）—— zhive Phase 1 是否要支持热重载？如果支持，listener 用 `WeakRef` 还是 scope token？

**Deliverable**：`deliverables/A5-extension-manifest.md`

- `Manifest` toml 字段表 × 12+（逐项标 Pi 源 / zhive 决定）
- 三命名空间 `kind: extension | prompt | skill` 字段定义
- 三层 `settingSources`（user/project/local）的合并规则
- `ResourcesDiscoverEvent` 是否进 Phase 1 的决定
- `renderCall / renderResult` 的 zhive 编码方案
- 热重载 listener 生命周期策略

---

## 5. Block B — zhive-core 实现调研

> 消费 Block A 的产出。每个任务的 deliverable 是 **module 草图 + 关键 trait 签名 + 与参考点的 diff**。

### B1 · Engine / agent loop + EnginePhase 状态机

**目的**：定义 `Engine` 的主循环、turn lifecycle、ownership 模型。**新增 `EnginePhase` 显式枚举（Pi 模式）取代隐式布尔**。

**参考点**：

- `${CODEX}/core/src/` 找 codex 的 engine.rs / runtime 等价物（codex 没有"engine"命名，看 `core/src/lib.rs` 顺藤摸瓜）
- `${CODEX}/core/src/thread_manager.rs` —— turn 调度逻辑
- `${PI}/packages/agent/src/harness/types.ts:485` —— `AgentHarnessPhase = "idle" | "turn" | "compaction" | "branch_summary" | "retry"`
- `${PI}/packages/agent/src/harness/agent-harness.ts:171` —— phase 字段持有 + 转换逻辑
- `${PI}/packages/agent/src/agent-loop.ts:160-268` —— 主循环结构

**关键问题**：

1. Engine 持有什么状态？（thread map / 当前 turn / cancel token / hook host / permission reducer / provider / **`phase: EnginePhase`**）
2. `EnginePhase` 枚举 case：抄 Pi 的 5 态（Idle/Turn/Compaction/BranchSummary/Retry），不设独立的 subagent phase —— subagent spawn 走 Turn 内的 `agent` 工具，不占 phase。
3. phase 转换的合法图（哪些转换允许 / 哪些非法）—— state machine 应该用 enum + match 还是 typestate？
4. 单 thread 内的 turn 是串行还是允许并发？（codex 怎么做的？）
5. Turn 的事件流（reasoning chunk → tool_call → tool_result → agent_message）如何在 engine 内组织？channel 拓扑？
6. ownership：`Engine` 是 `&self` 多读 还是 `Arc<Mutex<...>>` 还是 actor pattern？
7. phase 切换时的 hook 钩子（`PreCompact / PostCompact` 之外，是否需要 `PhaseTransition` 通用 hook？）

**Deliverable**：`deliverables/B1-engine-loop.md`

- `Engine` struct 字段 + 公开方法签名
- **`EnginePhase` enum 定义 + 合法转换图**
- Turn lifecycle 状态机图（Idle → Turn → 各子态 → Idle）
- 关键 channel 拓扑（mpsc / broadcast / oneshot 怎么分布）
- codex 同类型组件的并列对照表
- 与 Pi `AgentHarnessPhase` 的差异说明（zhive 加了什么 / 砍了什么 / 为什么）

### B2 · State 内存模型（Thread/Turn/Item）

**目的**：内存中怎么存活跃 thread/turn/item，与 A1 schema 解耦。

**参考点**：

- `${CODEX}/core/src/` 找 state.rs / session_store.rs
- 已有 `crates/zhive-core/src/state.rs` 骨架

**关键问题**：

1. Thread 的内存表征（`Arc<RwLock<Thread>>` vs `DashMap<ThreadId, ThreadHandle>`？）
2. Turn 历史 cap：长 session 时内存怎么不撑爆？（codex 有滚动 window 还是全量？）
3. Item 在 wire schema（A1）与内存 schema 之间是否同一类型？（D-006 说"单一 schema 源"——可能直接复用）

**Deliverable**：`deliverables/B2-state-memory-model.md`

- 内存类型草图
- 读 / 写 / 订阅访问路径（hooks 要订阅 item 流，怎么暴露）
- 与 persistence 层（B3）的 sync 点

### B3 · Persistence（sqlx **多库** + JSONL+Leaf rollout）

**目的**：[D-011 修订版](../../research/99-decisions/README.md#d-011-session-持久化--rusqlite-多库--jsonl-rollout) 落地。**4 库并行起步**，不走"单库 → 拆库"的演进路径。

**参考点**：

- `${CODEX}/state/` —— codex 当前多库实现，4 个 migrations 目录的最终蓝本：
  - `${CODEX}/state/migrations/`（35 文件，主 state DB：threads/logs/memories/agent_jobs）—— 注意这是**演进中段**，对照 `*_migrations` 子目录看哪些表已搬出
  - `${CODEX}/state/goals_migrations/`（1 文件）
  - `${CODEX}/state/logs_migrations/`（2 文件）—— logs 从主 DB 搬出
  - `${CODEX}/state/memory_migrations/`（1 文件）—— memories 从主 DB 搬出（PR #24591）
- `${CODEX}/state/src/lib.rs / paths.rs / migrations.rs / model/ / runtime/` —— 库管理 / 路径 / 迁移 / 模型
- `${CODEX}/rollout/src/` —— JSONL rollout crate（含 `recorder.rs / list.rs / search.rs / session_index.rs / state_db.rs`）；codex 自己也把 rollout 拆成独立 crate
- `${CODEX}/state/Cargo.toml` 用 **sqlx**（zhive 同样采 sqlx 0.8：`SqlitePool` 自带异步连接池，`sqlx::migrate!` 内嵌 SQL 文件）
- `${PI}/packages/agent/src/harness/session/jsonl-storage.ts:8-15` —— `SessionHeader { type: "session", version: 3, id, timestamp, cwd, parentSession? }`
- `${PI}/packages/agent/src/harness/types.ts:399-402` —— `LeafEntry { type: "leaf", targetId: string | null }` —— fork/branch 关键

**关键问题**：

1. **4 库分离的目录布局**（per D-011 修订）。base dir 解析顺序 `$ZHIVE_DATA_DIR` → `$XDG_DATA_HOME/zhive` → `$HOME/.local/share/zhive`：
   ```
   <base>/
     state.db
     logs.db
     memories.db
     goals.db
   crates/zhive-core/migrations/
     state/    *.sql
     logs/    *.sql
     memories/ *.sql
     goals/   *.sql
   ```
   每个 DB 的 `0001_*.sql` 初始 schema 应该长什么样？（对照 codex `*_migrations/0001_*.sql`）
2. `Storage` trait 4 子接口的具体方法签名（state: `append_item / list_threads / get_thread`；logs: `record_log / query_logs`；memories: `upsert_memory / search_memories`；goals: `add_goal / mark_done`）
3. 每库独立 `SqlitePool` 还是共享 pool？同进程下多 connection 的 `journal_mode = WAL` 行为？
4. **跨库事务**问题：`append_item`（state）+ `record_log`（logs）原子吗？不原子的话失败语义？
5. JSONL rollout 文件结构：路径布局（`<base>/rollouts/<sanitised thread_id>.jsonl`）+ 每行 schema
6. **Leaf 指针**（Pi 模式）写入策略：每次 append 后改 leaf 还是只在 fork 时改？fork 后旧 leaf 是否保留？
7. rollout 与 4 个 DB 的同步点：JSONL 是 source of truth，DB 是索引（崩溃后能否从 JSONL 重建 4 个 DB？应该可以，本任务给出 rebuild 流程）
8. sqlx 0.8 + 4 DB 文件下 cold build 时间 / 二进制体积（R-2 实测）

**Deliverable**：`deliverables/B3-persistence.md`

- 4 库 DDL（每库 `0001_*.sql`，对照 codex 同名 migration）
- JSONL 行 schema + Leaf entry schema + 文件路径布局
- `Storage` trait + 4 子 trait（`StateDb / LogsDb / MemoriesDb / GoalsDb`）草图
- sqlx `SqlitePool`（每库一池）的初始化与 `journal_mode = WAL` 配置
- 跨库一致性策略（"JSONL 总是先写成功，DB 失败可异步重建"还是其他）
- 4 DB 编译实测数据（cold build / 二进制体积 / 启动时 4 DB 打开耗时）
- 崩溃恢复流程：从 JSONL 重建 4 DB 的伪码
- 与 codex 当前实现的逐项对照：哪些表 zhive 抄 / 哪些重命名 / 哪些拒

### B4 · Server module（JSON-RPC over stdio + UDS）

**目的**：D-001 + D-003 + D-004 落地。`zhive-core::server` 模块（不是独立 crate）。

**参考点**：

- `${LSP}/src/server.rs` 或类似 —— stdio JSON-RPC 事件循环标杆
- `${LSP}/src/transport/` 找 stdin/stdout codec 实现
- `${CODEX}/app-server-transport/src/transport/` 看 stdio / unix_socket / websocket / remote_control 抽象
- `${CODEX}/app-server/src/` 看 server 启动 / 路由 / 中间件

**关键问题**：

1. `Transport` trait 接口（`AsyncRead + AsyncWrite` 还是更高层 message-stream？）
2. stdio transport 与 UDS transport 的区别只在 connect 侧还是更深？
3. Windows 第三 transport（lockfile + 127.0.0.1，D-004 决策）是否进 Phase 1 实现还是只占接口？
4. 请求路由（method name → handler）：硬编码 match 还是 registry？（tower-lsp 怎么做？）
5. 反向 RPC（server-initiated request）的 id 空间：与 client → server 共享 id pool 还是分离？
6. backpressure：客户端发太快怎么办？

**Deliverable**：`deliverables/B4-server-transport.md`

- `Transport` trait + `StdioTransport / UdsTransport` 实现要点
- 事件循环 main loop 伪码
- 反向 RPC 的 id 池设计
- 与 tower-lsp 的并列对照（"抄哪行 / 不抄哪行"）
- UDS socket 路径 / 权限 / 清理策略（绑定 D-004 `$XDG_RUNTIME_DIR/zhive.sock` 0600）

### B5 · Hook host

**目的**：D-012 host 侧落地（schema 在 A4 已定）。**含红线 10（hook source metadata）+ 红线 11（tool_call mutate 后必须重验证）**。

**参考点**：

- Claude Code SDK callback chain 文档
- `${PI}/packages/agent/src/harness/agent-harness.ts:391-401` —— `drainQueuedMessages` 失败时 `queue.unshift(...messages)` 回滚语义（zhive hook 失败时同样要回滚 pending state）
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:819` —— `tool_call` hook 允许 mutate `event.input` —— **Pi 反例：未重新验证 schema**
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:372-376` —— source metadata 字段（红线 10 直接抄字段名）
- `${PI}/packages/coding-agent/src/core/extensions/types.ts:551-557` —— `invalidate()` 生命周期（zombie listener 反例）
- ⚠️ `~/Desktop/code/github/cline/apps/vscode/src/core/hooks`（如果有 clone）—— **反例**

**关键问题**：

1. Hook 注册时机：startup 一次性扫盘 vs 每次 turn 重扫？
2. Hook 执行模型：进程内 trait + JSON 反序列化（in-process），还是 spawn 子进程？两者怎么共存？
3. 多个 hook 对同一 event 的执行顺序：注册顺序 / 按 namespace / 按 priority？
4. Hook timeout / panic 隔离：一个挂了怎么不连累 turn？
5. 与 permission reducer（B6）的协作点（PreToolUse hook 返回 PermissionDecision，谁负责 fold？）
6. **红线 10 落地**：每个注册 hook 必带 `registered_by: ExtensionRef`（`{ id, version, source: "user" | "project" | "local" | "builtin" }`）—— `register_hook(...)` API 怎么 enforce？
7. **红线 11 落地**：`tool_call` hook mutate `event.input` 后 host 必须再过一次 schema 验证；失败时回滚到 mutate 前还是 abort turn？
8. Hook 失败时 pending queue 回滚（Pi 的 unshift 语义）—— 在 zhive 的 `queue.splice → fn(messages) → on Err: queue.unshift_front(messages)` 用什么数据结构？
9. Listener 生命周期：`invalidate()` 后僵尸 listener 防护（用 `Arc<Weak<dyn HookFn>>` 还是显式 scope token？）

**Deliverable**：`deliverables/B5-hook-host.md`

- `HookHost` trait + 内置实现
- `ExtensionRef` 结构定义 + `register_hook` API 签名
- 注册 / 调度 / 错误隔离的状态机
- pending queue 回滚机制（参照 Pi `drainQueuedMessages`）
- mutate 后重验证 schema 的强制流程
- zombie listener 防护方案 + lifetime annotation
- 14 个 event 各自的"谁触发 / 谁消费 / 是否允许 mutate"对照表

### B6 · Permission reducer

**目的**：D-008 reducer 侧落地（schema 在 A3 已定）。

**参考点**：

- A3 deliverable
- Claude Code reducer 描述

**关键问题**：

1. 多 hook 并行 fold：是否真的并行（join_all）还是 sequential？两种语义差异
2. Subagent 父子两侧各 reduce 一次：怎么传值？谁触发？
3. defer 怎么实现？需要 user 后续输入再继续 —— UI / RPC 路径

**Deliverable**：`deliverables/B6-permission-reducer.md`

- `Reducer` 函数签名 + 实现伪码
- 父子 subagent 调用图
- defer 后的 follow-up RPC 流程图

### B7 · 取消传播 + StreamingBehavior 状态机 + pendingSessionWrites

**目的**：R5 finding #1 + D-008 修订版（三队列）落地。**含 Pi `pendingSessionWrites` 智能刷新机制**。

**参考点**：

- A3 deliverable（schema：三队列 + QueueMode）
- `${ACP}/rust/src/lib.rs` `session/cancel`
- `${LSP}/src/` 找 cancel token / `$/cancelRequest` 实现
- `${PI}/packages/agent/src/harness/agent-harness.ts:439-450` —— `pendingSessionWrites` push 机制（phase ≠ idle 时入 buffer）
- `${PI}/packages/agent/src/harness/agent-harness.ts:174` —— `flushPendingSessionWrites()` 在 save point 统一 drain（按类型分发到对应 session 方法）
- `${PI}/packages/agent/src/harness/agent-harness.ts:552-565` —— AbortSignal 传播到长生命周期操作（compaction / tree navigation）
- `${PI}/packages/coding-agent/docs/hooks.md:23-25` —— hook 签名 `(event, ctx, signal?) => ...`

**关键问题**：

1. `CancellationToken` 来源（`tokio_util::sync::CancellationToken`？已在 workspace 依赖？）
2. 取消信号传播树：Engine → Turn → ToolCall → Hook → Subagent → **compaction / branch_summary**（Pi 长操作）—— 哪些是 token clone 哪些是显式 channel？
3. **Steer 不撤销 in-flight tool_call**（Pi 模式）—— 这与 R5 finding #1 假设不同，A3 + B7 要明确：当前 turn 工具继续执行，steer 消息在下轮 LLM 请求才生效
4. Reverse-request 回收（pending Map 清理）的 ownership
5. **`PendingSessionWrites` buffer 机制**：phase ≠ idle 时所有 session 写入入 buffer，save point 统一 flush —— zhive 是否要全面采纳？还是只在 compaction phase 用？
6. Hook signature 的 `signal: Option<&CancellationToken>` 第三参数：是否必填？compaction hook / tree navigation hook 没 signal 怎么响应 abort？

**Deliverable**：`deliverables/B7-cancel-streaming.md`

- 取消传播树图（含 compaction / branch_summary 长操作分支）
- Steer / FollowUp / NextTurn 三队列各自的时序图
- pending reverse-request Map 的 lifecycle
- `PendingSessionWrites` buffer + flush 机制设计（zhive 版本）
- Hook signature 决定（带不带 signal / 怎么传）
- "Steer 不撤销当前 tool_call" 的对外文档措辞

### B8 · Subagent 调度

**目的**：D-008 + Claude Code 三条硬约束（fresh window / only final message / 禁递归）。

**参考点**：

- Claude Code Subagents docs

**关键问题**：

1. Subagent 是新 Engine instance 还是同 Engine 内新 Thread？（fresh context window 怎么实现得最便宜）
2. 禁递归：在 spawn 入口拦还是在 schema 层禁？（前者简单后者更严）
3. parent → child 的 prompt 字符串 / 工具白名单怎么传？
4. child → parent 的 final message 怎么回？什么是"final"？

**Deliverable**：`deliverables/B8-subagent.md`

- Subagent spawn 流程
- 禁递归的实现位置
- 与 B6 父子继承的对接点

### B9 · tracing spans

**目的**：D-014 落地。

**参考点**：

- D-014 决策
- `tracing` crate docs
- `tracing-opentelemetry` （仅作为 feature gate 调研，不进 Phase 1 必装）

**关键问题**：

1. 6 个必覆盖 span（Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint）的 instrument 位置
2. span 字段命名（`thread_id / turn_id / tool_name / ...`）的统一规则
3. 何时使用 `error!` / `warn!` / `info!`（CLAUDE.md 没规定，本任务定下）
4. tracing-subscriber 初始化在哪一层（cli 入口 / core 入口）

**Deliverable**：`deliverables/B9-tracing.md`

- span 矩阵：组件 × span 名 × 字段
- 日志级别约定
- subscriber 初始化点

### B10 · LLM provider 抽象（Phase 1 占位）

**目的**：R5 finding #4 列的"Phase 1 占位决策缺失"中的 provider 一项。

**参考点**：

- `llmsdk` crate（已在项目 git 依赖中）—— 调研其 trait 表面
- ⚠️ `~/Desktop/code/github/cline/providers/*.ts` —— **反例**：每加一个 provider 一个 PR

**关键问题**：

1. `llmsdk` 已有的 trait 能不能直接当 zhive 的 provider boundary？
2. 流式响应（reasoning / agent_message chunk）的接口形状
3. tool_call schema 在 provider 层 vs zhive Item schema 的转换点

**Deliverable**：`deliverables/B10-provider.md`

- 决定：直接复用 `llmsdk` trait / 包一层 zhive adapter / 自定义 trait
- provider → zhive Item 的转换草图

---

## 6. Block C — zhive-client-native 实现调研

> 仅依赖 proto。Block A 完成后即可独立推进，与 Block B 并行。

### C1 · Client API 表面

**目的**：定义 native client 的公开方法 / 类型。

**参考点**：

- `${CODEX}/app-server-client/src/lib.rs`（codex Rust client lib 入口）
- `${CODEX}/app-server-client/` 整个目录的 API surface

**关键问题**：

1. `Client::connect_stdio(child: Child) / connect_uds(path) / connect_remote(url)` —— 三种 connect 是否共一个 builder？
2. 同步 API（`request().await`）vs streaming API（`subscribe(method) -> Stream<Notification>`）—— 分离还是融合？
3. Reverse-request 由 client 谁处理？（trait `ReverseRequestHandler`？还是预注册 closure？）
4. Drop 行为：连接异常时是否 panic、还是返回 error stream？

**Deliverable**：`deliverables/C1-client-api.md`

- `Client / ClientBuilder` 公开 API 草图
- 与 codex `app-server-client` 的字段对照表
- 同步 / 流式 / 反向 三类 API 的拓扑

### C2 · 连接管理 / 重连

**目的**：处理连接断开 / 进程死亡 / UDS 文件消失。

**参考点**：

- `${CODEX}/app-server-client/` 找 reconnect 逻辑
- `${LSP}/src/` 看 server 死亡时 client 行为

**关键问题**：

1. Phase 1 是否做自动重连？（codex 怎么做？）
2. 重连后已发出未回的请求怎么办（cancel / retry / 直接 error）？
3. UDS 路径在重启后变了怎么发现？

**Deliverable**：`deliverables/C2-reconnect.md`

- 连接 lifecycle 状态机
- 在线 / 离线 时 pending request 处理策略

### C3 · 取消处理

**目的**：client 侧的取消能正确触达 server。

**参考点**：

- B7 deliverable
- ACP `session/cancel`
- `${LSP}/src/` `$/cancelRequest`

**关键问题**：

1. `Client::request()` 返回值是否带 `CancellationToken`？
2. `client.cancel(turn_id)` 是单独 RPC 还是 notification？
3. drop in-flight future 时是否自动发 cancel？

**Deliverable**：`deliverables/C3-client-cancel.md`

- 取消 API 形状
- 与 server 侧（B7）的对接点

### C4 · 反向 RPC handler 注册

**目的**：server 发起的 `permission/request / hook/invoke` 等，client 怎么响应。

**参考点**：

- `${ACP}/rust/src/lib.rs` 找 `permission/request` 客户端侧处理
- `${LSP}/src/` 看 reverse request（`$/`系列）的注册

**关键问题**：

1. handler 注册接口：trait 实现 / `register_handler(method, fn)` / typed 注册（每个 method 一个 fn）？
2. handler 执行环境（同步 / async / spawn_blocking）
3. 未注册的 reverse method 默认行为（自动 deny / error / panic）

**Deliverable**：`deliverables/C4-reverse-handler.md`

- 注册 API 草图
- 反向 method × 默认行为 表
- 与 ACP `permission/request` 形状的对齐验证

---

## 7. 调研顺序与验收

### 7.1 DAG

```
A1 ─┬─> B1 ─> B2 ─> B3
    │
A2 ─┼─> B4
    │
A3 ─┼─> B6
    │    └─> B7
    │
A4 ─┼─> B5
    │
A5 ─┘

A1 + A2 + A3 ─> C1 ─> C2 ─> C3 ─> C4

B1 ─> B8  (subagent 依赖 engine)
B1 ─> B9  (tracing 依赖知道 span 该插哪)
B1 ─> B10 (provider 调用点在 engine)
```

**关键路径**：A1 → B1 → 后续全部。先把 A1 + A2 + A3 三块 schema 砸实，B/C 才有地基。

### 7.2 调研执行建议

- **A1 / A2 / A3 / A4 / A5**：5 个独立 subagent 并行（互不依赖）
- **B1 / B4**：等 A 全部完成后启动（B1 依赖 A1，B4 依赖 A2）
- **B2 / B3 / B5 / B6 / B7 / B8 / B9 / B10**：B1 完成后可大幅并行
- **C1 ~ C4**：与 B 并行，只需 A 完成

### 7.3 每任务的验收条件（统一）

deliverable 必须包含：

1. **参考点清单**：列出读过的具体文件路径 + 行号或 PR 号（每个论断都有锚点）
2. **设计选择**：每个分叉给出选 A 不选 B 的理由（用上面"关键问题"列表逐条回答）
3. **Rust 草图**：trait / struct / enum 的 Rust 代码，至少能 `cargo check`（不实现，`todo!()` 占位）
4. **未决项标注**：用 `> TODO(开放项)：...` 行内标记，方便回流到 [§ 9 开放项](#9-风险与开放项)

### 7.4 全局退出条件

调研完成 = 全部 Block A/B/C 的 deliverables 写完 + 串读一遍能拼出 zhive-core / zhive-client-native 的完整文件树。一个反向检验：能不能给一个**没看过 5 个 repo 的工程师**直接派活写 `Engine::run_turn()`？如果不能，回到对应 deliverable 补。

---

## 8. 调研基线

为防止 upstream 漂移，调研开始前在每个 cloned repo 跑：

```bash
for r in codex acp-rust-sdk rmcp tower-lsp rusqlite; do
  echo "=== $r ==="
  git -C ~/Desktop/code/github/$r log --oneline -1
done
```

结果记到 `plans/phase1-core-native-research/baseline-commits.md`，**调研期间不 `git pull`**。如必须升级，写 `baseline-commits.md` 一笔附"为什么升 + 受影响的 deliverable"。

---

## 9. 风险与开放项

| ID | 项 | 来源 | 应对 |
|---|---|---|---|
| ~~R-1~~ | ~~Pi CLI 仓库地址未知~~ | ~~92 § 六 + R6~~ | ✅ **2026-05-28 解决**：Pi 仓库已 clone 至 `${PI}`；A3/A4/A5/B1/B3/B5/B7 已直接引用具体行号 |
| R-2 | sqlx 0.8 + **4 库** 编译开销可能比估算大 | D-011 修订；D-009 估算未含多库 | B3 必须实测一次 cold build + 二进制体积，超 60s 或体积 > 30MB 就向用户确认是否回退 |
| R-3 | tower-lsp 已经数年无更新（最后 release 2022），事件循环模式是否还代表"现代 stdio JSON-RPC 标杆"？ | clone 列表 | B4 先扫一眼 last commit，若太旧考虑 alternative（async-lsp / lsp-server） |
| R-4 | `llmsdk` 内部 trait 表面未审，B10 可能给不出"直接复用"结论 | B10 | 若发现不能直接用，本调研只产出"差距清单"，trait 设计推到后续 |
| R-5 | A1 决定的 `Item` enum 字段若后续与 ACP / MCP runtime 冲突 | D-005 已承诺 bridge crate 内适配 | 容忍：bridge 侧适配是 D-005 既定路线，不为此修 A1 |
| R-6 | OTel 三 crate 已声明但 `otel` feature 默认关闭，Phase 1 不激活；span 字段命名若不按 OTel 规约会后悔 | D-014 | B9 提前查 OTel semantic conventions，字段名按 OTel 起 |
| R-8 | 4 库跨库事务无原子保证 | D-011 修订 | B3 给 fail-strategy；JSONL 始终先写成功，DB 失败可异步重建（参照 § 崩溃恢复） |
| R-9 | Pi 反例：`tool_call` mutate input 后未重验证 schema → 工具崩溃 | Pi 调研 | 红线 11 已上锁；B5 必须落地 mutate 后强制 re-validate |
| R-10 | Pi 反例：缺 hook source metadata 导致后续无法定位注册者 | Pi 调研 | 红线 10 已上锁；A4 + B5 双向落地 `registered_by` 必填 |
| R-11 | Pi 反例：`invalidate()` 后 zombie listener 残留 | Pi 调研 | B5 给 listener lifetime 策略（`Weak` / scope token） |

---

## 10. 与决策文档的回流

调研期间若发现某 D-00X 决策**与一手代码冲突**或**字段命名需修订**，**不直接改决策文档**，而是：

1. 在对应 deliverable 顶部用 `> 决策冲突警告：D-00X 说 ...，但 ${参考} 实测 ...，建议 ...` 标记
2. 全部调研完成后，集中产出 `plans/phase1-core-native-research/decision-diffs.md`
3. 由用户决定是否更新 `research/99-decisions/README.md`

这避免调研过程中决策文档反复改导致回滚困难。

---

## 11. 产物索引（调研完成时填）

| Block | 任务 | Deliverable | 状态 |
|---|---|---|---|
| A | A1 三层原语 | `deliverables/A1-thread-turn-item.md` | ✅ |
| A | A2 initialize/capabilities | `deliverables/A2-initialize-capabilities.md` | ✅ |
| A | A3 permission/streaming/subagent | `deliverables/A3-permission-streaming-subagent.md` | ✅ |
| A | A4 hook events | `deliverables/A4-hook-event-schema.md` | ✅ |
| A | A5 extension manifest | `deliverables/A5-extension-manifest.md` | ✅ |
| B | B1 engine loop | `deliverables/B1-engine-loop.md` | ✅ |
| B | B2 state memory | `deliverables/B2-state-memory-model.md` | ✅ |
| B | B3 persistence | `deliverables/B3-persistence.md` | ✅ |
| B | B4 server/transport | `deliverables/B4-server-transport.md` | ✅ |
| B | B5 hook host | `deliverables/B5-hook-host.md` | ✅ |
| B | B6 permission reducer | `deliverables/B6-permission-reducer.md` | ✅ |
| B | B7 cancel/streaming | `deliverables/B7-cancel-streaming.md` | ✅ |
| B | B8 subagent | `deliverables/B8-subagent.md` | ✅ |
| B | B9 tracing | `deliverables/B9-tracing.md` | ✅ |
| B | B10 provider | `deliverables/B10-provider.md` | ✅ |
| C | C1 client API | `deliverables/C1-client-api.md` | ✅ |
| C | C2 reconnect | `deliverables/C2-reconnect.md` | ✅ |
| C | C3 client cancel | `deliverables/C3-client-cancel.md` | ✅ |
| C | C4 reverse handler | `deliverables/C4-reverse-handler.md` | ✅ |
| 全局 | baseline 锚点 | `baseline-commits.md` | ✅ |
| 全局 | 决策修订建议 | `decision-diffs.md` | ✅ |
