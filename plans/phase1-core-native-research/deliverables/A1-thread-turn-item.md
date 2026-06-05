---
task: A1
title: 三层原语 Thread / Turn / Item domain schema（D-006 落地）
date: 2026-05-28
status: implemented
depends_on:
  - research/99-decisions D-006 (Thread/Turn/Item + serde+schemars 单 schema 源)
  - research/99-decisions D-005 (rmcp/ACP 仅在 bridge crate)
  - research/99-decisions D-007 (initialize / v1+v2 / capabilities)
references:
  - ${CODEX}/app-server-protocol/src/protocol/v2/thread_data.rs  (Thread / Turn / TurnStatus / TurnItemsView / TurnError)
  - ${CODEX}/app-server-protocol/src/protocol/v2/item.rs         (ThreadItem enum 17 case 的全集；Item builder)
  - ${CODEX}/app-server-protocol/src/protocol/v2/turn.rs         (TurnStartParams / TurnSteerParams / TurnInterruptParams / UserInput / TurnStarted/CompletedNotification)
  - ${CODEX}/app-server-protocol/src/protocol/v1.rs              (v1 thread schema 留作对照)
  - ${ACP}/src/agent-client-protocol/src/schema/agent_to_client/notifications.rs (session/update 入口)
  - agent-client-protocol-schema 0.12.0/src/client.rs             (SessionUpdate enum 10 case + ContentChunk)
  - agent-client-protocol-schema 0.12.0/src/tool_call.rs          (ToolCall / ToolCallUpdate / ToolKind / ToolCallStatus / ToolCallContent / Content / Terminal / Diff / ToolCallLocation)
  - agent-client-protocol-schema 0.12.0/src/content.rs            (ContentBlock 5 case)
  - agent-client-protocol-schema 0.12.0/src/lib.rs                (SessionId 类型定义)
  - ${RMCP}/crates/rmcp/src/model/content.rs                      (RawContent enum 5 case)
  - crates/zhive-proto/src/lib.rs                                  (已就位 JSON-RPC envelope)
  - crates/zhive-core/src/state.rs                                 (待补 Thread/Turn/Item)
---

> 说明：以下所有“ACP schema”锚点都引自 `agent-client-protocol-schema v0.12.0`（本调研基线，本地源码位于 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-schema-0.12.0/`）；运行时依赖 `agent-client-protocol = "0.13"`。

---

## 1. 参考点清单

每个论断的锚点（repo + 文件 + 行号），下文逐条引用：

| 主题 | 路径 | 行号 |
|---|---|---|
| `Thread { id, session_id, forked_from_id, preview, ephemeral, model_provider, created_at, updated_at, status, path, cwd, cli_version, source, thread_source, agent_nickname, agent_role, git_info, name, turns }` | `${CODEX}/app-server-protocol/src/protocol/v2/thread_data.rs` | 102-148 |
| `Turn { id, items, items_view, status, error, started_at, completed_at, duration_ms }` | `${CODEX}/app-server-protocol/src/protocol/v2/thread_data.rs` | 150-172 |
| `TurnItemsView = NotLoaded \| Summary \| Full` | `${CODEX}/app-server-protocol/src/protocol/v2/thread_data.rs` | 174-185 |
| `TurnError { message, codex_error_info, additional_details }` (`#[derive(Error)]`) | `${CODEX}/app-server-protocol/src/protocol/v2/thread_data.rs` | 187-196 |
| `TurnStatus = Completed \| Interrupted \| Failed \| InProgress` | `${CODEX}/app-server-protocol/src/protocol/v2/turn.rs` | 25-33 |
| `ThreadItem` 17 case enum `#[serde(tag = "type", rename_all = "camelCase")]` | `${CODEX}/app-server-protocol/src/protocol/v2/item.rs` | 208-363 |
| `ThreadItem::id()` accessor 模式 | `${CODEX}/app-server-protocol/src/protocol/v2/item.rs` | 373-394 |
| `TurnStartedNotification { thread_id, turn }` / `TurnCompletedNotification { thread_id, turn }` | `${CODEX}/app-server-protocol/src/protocol/v2/turn.rs` | 350-373 |
| `TurnStartParams { thread_id, input: Vec<UserInput>, … }`（开启 Turn 的入口） | `${CODEX}/app-server-protocol/src/protocol/v2/turn.rs` | 61-144 |
| `TurnInterruptParams { thread_id, turn_id }` | `${CODEX}/app-server-protocol/src/protocol/v2/turn.rs` | 181-192 |
| ACP `SessionUpdate` 10 case enum `#[serde(tag = "sessionUpdate", rename_all = "snake_case")]` `#[non_exhaustive]` | agent-client-protocol-schema-0.12.0/src/client.rs | 75-115 |
| ACP `SessionNotification { session_id, update, _meta }` wrapper | agent-client-protocol-schema-0.12.0/src/client.rs | 38-51 |
| ACP `ContentChunk { content, _meta }` | agent-client-protocol-schema-0.12.0/src/client.rs | 340-366 |
| ACP `ToolCall { tool_call_id, title, kind, status, content, locations, raw_input, raw_output, _meta }` | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 22-59 |
| ACP `ToolKind` enum (Read/Edit/Delete/Move/Search/Execute/Think/Fetch/SwitchMode/Other) | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 384-416 |
| ACP `ToolCallStatus` enum (Pending/InProgress/Completed/Failed) | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 425-444 |
| ACP `ToolCallContent` 3 case (Content/Diff/Terminal) `#[serde(tag = "type", rename_all = "snake_case")]` | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 453-474 |
| ACP `Diff { path, old_text, new_text, _meta }` | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 572-590 |
| ACP `Terminal { terminal_id, _meta }` | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 531-544 |
| ACP `ToolCallLocation { ... }` | agent-client-protocol-schema-0.12.0/src/tool_call.rs | 632 |
| ACP `ContentBlock` 5 case enum `#[serde(tag = "type", rename_all = "snake_case")]` (Text/Image/Audio/ResourceLink/Resource) | agent-client-protocol-schema-0.12.0/src/content.rs | 32-60 |
| ACP `SessionId(pub Arc<str>)` `#[serde(transparent)]` `#[non_exhaustive]` | agent-client-protocol-schema-0.12.0/src/lib.rs | 99-110 |
| ACP `session/update` notification 注册位置 | ${ACP}/src/agent-client-protocol/src/schema/agent_to_client/notifications.rs | 1-3 |
| MCP `RawContent` 5 case enum `#[serde(tag = "type", rename_all = "snake_case")]` (Text/Image/Resource/Audio/ResourceLink) `#[expect(clippy::exhaustive_enums)]` | ${RMCP}/crates/rmcp/src/model/content.rs | 149-160 |
| `MCP 无 Turn 概念 → bridge 侧合成 Turn 边界（在 tools/call 入口起、CallToolResult 收）` | research/99-decisions/README.md (D-006) | 173 |
| `Thread ↔ ACP Session 桥接表 + ID 命名空间（不是 1:1）` | research/99-decisions/README.md (D-006) | 164 |
| zhive-proto 已就位的 envelope | crates/zhive-proto/src/lib.rs | 45-110 |

---

## 2. 关键设计选择（三大关键问题逐条作答）

### 2.1 Q1：`Item` enum 全集 case，字段名对齐谁？

**决策**：`Item` 以 **codex v2 ThreadItem 的 17 case 子集为骨架**，**叶子 content 字段对齐 ACP `ContentBlock` / `ToolCallContent`** （后两者是 zhive bridge 必须互译的字段，对齐过去 1:1 同构，反过来对齐 codex 则要在 bridge 内做转码）。

**两套候选对齐源对比**：

| 候选 | 优点 | 缺点 |
|---|---|---|
| A. 完全对齐 codex v2 `ThreadItem` 17 case | 字段名 / discriminator (`type`) / TS export 已成熟；codex 是单 schema 源最早期的范本，工程量已验证 | 字段集合很大（含 `CollabAgentToolCall / WebSearch / EnteredReviewMode / ContextCompaction` 这些 zhive Phase 1 用不到的特性）；引入 `codex_protocol::config_types` 等强耦合类型 |
| B. 完全对齐 ACP `SessionUpdate` 10 case | bridge-stdio Phase 1 必交付，对齐过去映射成本最低 | `SessionUpdate` 是**“event 流”单位**，不是“项目库”单位（混了 chunk / mode_change / available_commands / config_option 等非内容性事件）；做不到 D-006 "Item 是项目内容"语义 |
| C（选）. **以 codex `ThreadItem` 形状为骨架，叶子 content 字段直接对齐 ACP `ContentBlock`/`ToolCallContent`** | 项目内容性语义 + bridge 映射 1:1 同构（D-006 §依据已实测 ~150-250 行）；舍弃 codex 中 `CollabAgent*` 等私有概念；保留 D-006 已锁定的 `Diff / Terminal / Thought` 三 ACP `ToolCallContent` 承载位 | 字段命名要做一层"组合源"标注（下面 §3 表已列） |

**zhive `Item` 全集 14 case**（基于 D-006 §决策的 8 case 起步 + 4 个 D-006 已锁定但 ACP 必需的承载位 + 2 个 codex 必需的运行时状态项）：

> ⚠️ D-006 决策文档原文列了 8 个 case（`reasoning / tool_call / exec / file_edit / agent_message / diff / terminal / thought`）。下面 14 case 是把 D-006 列的 8 个 + ACP/codex 实测必需补的 6 个合并。决策修订记号下挂到 §7 未决项。

```text
1. UserMessage          # 用户输入（D-006 未列；codex `UserMessage` + ACP `UserMessageChunk` 必需）
2. AgentMessage         # 助手回复（D-006 「agent_message」）
3. Reasoning            # 推理（D-006 「reasoning」）
4. AgentThought         # 内部思考（D-006 「thought」；ACP「AgentThoughtChunk」）
5. ToolCall             # 工具调用（D-006 「tool_call」；含完整 ACP ToolCall 字段集）
6. CommandExecution     # shell 执行（D-006 「exec」；对应 codex CommandExecution / ACP 内嵌 Terminal）
7. FileEdit             # 文件编辑（D-006 「file_edit」；对应 codex FileChange / ACP ToolCallContent::Diff）
8. Diff                 # 文件 diff（D-006 「diff」；独立承载位，仅在 ToolCall 内不便表达时使用）
9. Terminal             # 终端嵌入（D-006 「terminal」；ACP ToolCallContent::Terminal）
10. Plan                # 计划（D-006 未列；ACP「Plan」+ codex「Plan」均有，bridge 不映射会丢字段）
11. AvailableCommands   # 可用命令快照（codex 无，ACP `AvailableCommandsUpdate` 必需；bridge 映射目标）
12. ModeChange          # 模式切换（codex 无，ACP `CurrentModeUpdate` 必需）
13. ContextCompaction   # 上下文压缩（D-006 未列，codex 有；A4 hook `PreCompact / PostCompact` 必有对照 item）
14. SystemNotice        # 系统通知（zhive 自有；D-012 「Notification」hook 触发后落地为 item）
```

> TODO(开放项 OP-1)：D-006 决策只列 8 case，但 ACP/codex 实测下界需要 14 case 才不丢字段。建议 plan §10 回流时把 D-006 的 8 case 扩到 14 case，并明确「Item 字段集 = bridge 必需映射点的交集」原则。

**字段命名规则**：

- **manifest / wire JSON**：camelCase（与 ACP `#[serde(rename_all = "camelCase")]` / codex v2 `#[serde(rename_all = "camelCase")]` 双方一致）
- **Rust 内部类型**：PascalCase enum case + snake_case 字段
- **discriminator**：`#[serde(tag = "itemKind")]`（值用 `snake_case`，与 D-006 §决策 "rename_all = snake_case" 习惯对齐；codex 用 `type` + camelCase，ACP 用 `sessionUpdate` + snake_case，**zhive 选 `itemKind` 避开两个 keyword 复用**）

> 决策冲突来源：codex v2 `ThreadItem` 用 `#[serde(tag = "type")]`（`item.rs:209`），ACP `SessionUpdate` 用 `#[serde(tag = "sessionUpdate")]`（`client.rs:81`），rmcp `RawContent` 用 `#[serde(tag = "type")]`（`content.rs:150`）。zhive 选 `itemKind` 是为了：(a) 不与 ACP `sessionUpdate`、codex/rmcp `type` 任一冲突（bridge 不会同 key 撞名）；(b) `type` 在 JSON Schema 里和 schemars 的 `"type"` 元字段易混。叶子 enum（`ItemContent` / `ItemToolCallContent`）则沿用 ACP 的 `tag = "type"` 以保持 1:1 wire 兼容。

### 2.2 Q2：`Thread.id ↔ ACP.SessionId` namespace 设计（D-006 「桥接表 + ID 命名空间」具体怎么编码？）

**决策**：使用 **uri-style 命名空间字符串 + 桥接表**。`ThreadId` 是 zhive 内的强类型 newtype（`pub struct ThreadId(pub Arc<str>);`），其字符串形态遵循：

```
thread:<provenance>/<uuid-v7>
```

- `provenance` 取值：`native | acp | mcp`（与 D-005 三类 bridge 对齐；`native` = 由 zhive-cli 直接发起）
- `<uuid-v7>` 是按时间排序的 uuid（便于 SQLite 索引 + JSONL append 序）

`SessionId`（ACP wire 类型）保留 ACP 原样字符串（`Arc<str>`，见 `agent-client-protocol-schema-0.12.0/src/lib.rs:99-110`），通过 **桥接表**与 `ThreadId` 多对一：

```rust
// 桥接表（D-006 字面落地，存于 state.db；详细 DDL 推到 B3）
struct ThreadAcpBinding {
    thread_id: ThreadId,        // PK 之一（多 ACP 会话可绑同一 Thread）
    acp_session_id: AcpSessionId, // String wrapper of ACP SessionId
    bridge_kind: BridgeKind,    // enum { Acp, McpSynthesized }
    created_at: i64,
}
```

**为何不 1:1**（D-006 「不是 1:1」字面要求）：
1. ACP 客户端可以发 `session/load` 同 SessionId 多次，但每次复用同一 Thread —— ACP `SessionId` 在客户端语义里是“会话句柄”，不是“持久会话”
2. MCP bridge 没有 session 概念，要为每个 `tools/call` 序列**合成**一个 ACP-style 的 SessionId（D-006 §依据「在 tools/call 入口起、CallToolResult 收」），这种合成 SessionId 同 Thread 多对一
3. fork（codex `forked_from_id`，`thread_data.rs:110`）会产生子 Thread，但要保留 ACP SessionId 不变（fork 是 zhive 内部行为）

**选 A（uri-style）不选 B（裸 uuid）的理由**：
- uri-style 让 grep 友好（`grep '^thread:acp/' rollouts/*.jsonl`）；裸 uuid 调试地狱
- uri-style 在 wire 上仍是单字符串（`#[serde(transparent)]`），ACP/MCP 客户端不感知
- 解析失败时 fallback 到 `thread:unknown/<raw>`，不 panic

### 2.3 Q3：MCP 侧无 Turn → core 怎么暴露 "Turn 开始 / 结束" 给 bridge？

**决策**：core 暴露 **JSON-RPC notification 一对** `events/turn_started` / `events/turn_completed`（语义对照 codex `turn/started` / `turn/completed`，见 `turn.rs:350-373`），bridge-stdio 订阅这两个 notification 并在自己内部映射成「`tools/call` 序列开始 / 结束」。

bridge 侧合成 Turn 的具体动作：
1. **`Server::handle_session_prompt` 进入时**：core 自动 `engine.start_turn(thread_id, user_inputs)` → 发 `events/turn_started` notification（payload = `TurnStartedNotification { thread_id, turn }`）
2. **agent loop 结束时**（无更多 LLM 调用 / 全部 tool_call 完成 / cancel 触发）：core `engine.finish_turn(turn_id, status)` → 发 `events/turn_completed` notification（payload 含 `TurnStatus` 四态：`Completed / Interrupted / Failed / InProgress`，但 `InProgress` 永远不会出现在 completed 通知中，仅作为 read-time 状态）
3. **bridge-mcp（Phase 2）**：监听 `events/turn_started` → 启动一段 stdout 缓冲；监听 `events/turn_completed` → 把缓冲打包成单个 `CallToolResult` 返回给 MCP 客户端

**为何选 notification 而非 subscribe-only stream**（D-006 没指定）：
- D-003 已锁定 JSON-RPC 2.0（notification 是 spec § 4.1 标准 message type）；不需要新机制
- bridge 进 / 出都用 notification 与 D-008「反向 RPC = JSON-RPC server-initiated request」对齐
- 多客户端订阅同 Thread 时，notification 是 fan-out 友好的（每个连接独立 sink）

**Turn boundary detection 算法**（伪码）：

```text
on session/prompt(thread_id, user_inputs):
    turn = state.start_turn(thread_id, user_inputs)
    emit notification { method: "events/turn_started", params: TurnStartedNotification { thread_id, turn } }
    spawn agent_loop(turn.id):
        loop:
            if cancel.is_cancelled(): break Interrupted
            llm_resp = provider.send(...).await?
            for item in llm_resp.items:
                state.append_item(turn.id, item)  // 同步发 item-level notification
                if item is ToolCall: dispatch_tool_call(item).await
            if llm_resp.is_final: break Completed
    emit notification { method: "events/turn_completed", params: TurnCompletedNotification { thread_id, turn: turn.with_status(...) } }
```

---

## 3. 字段命名表（zhive → 对齐源）

> 三列：zhive 字段（Rust 字段名，snake_case） / 对齐源 / 备注

### Thread

| zhive 字段 | 对齐源 | 备注 |
|---|---|---|
| `id: ThreadId` | codex `Thread.id`（`thread_data.rs:106`） | wire `thread:<provenance>/<uuid-v7>`；详见 §2.2 |
| `session_id: Option<SessionId>` | codex `Thread.session_id`（`thread_data.rs:108`） + ACP `SessionId`（`lib.rs:99-110`） | 仅当 Thread 由外部 ACP/MCP 会话挂入时填；native client 直发为 `None` |
| `forked_from: Option<ThreadId>` | codex `forked_from_id`（`thread_data.rs:110`） | fork 链 |
| `preview: String` | codex `preview`（`thread_data.rs:112`） | 首条用户消息（截断 200 char） |
| `ephemeral: bool` | codex `ephemeral`（`thread_data.rs:114`） | true ⇒ 不落 JSONL |
| `model_provider: String` | codex `model_provider`（`thread_data.rs:116`） | 与 B10 provider 抽象对接 |
| `created_at: i64` | codex `created_at`（`thread_data.rs:119`） | unix ts（秒） |
| `updated_at: i64` | codex `updated_at`（`thread_data.rs:122`） | unix ts（秒） |
| `status: ThreadStatus` | codex `status`（`thread_data.rs:124`） + `ThreadStatus`（`thread.rs:1106-1119`） | `NotLoaded / Idle / Active{active_flags} / SystemError` |
| `cwd: PathBuf` | codex `cwd`（`thread_data.rs:128`） | 工作目录 |
| `source: ThreadSource` | codex `thread_source`（`thread_data.rs:67-71` / `thread_data.rs:134`） | `User / Subagent / MemoryConsolidation` |
| `name: Option<String>` | codex `name`（`thread_data.rs:142`） | 用户可改的 thread 标题 |
| `turns: Vec<Turn>` | codex `turns`（`thread_data.rs:147`） | 仅在 `thread/read?includeTurns=true` 等响应里 populate |

**砍掉 codex 字段**（zhive Phase 1 不需要）：
- `path: Option<PathBuf>` —— D-011 把 JSONL 路径标准化为 `<data_dir>/rollouts/<thread_id>.jsonl`（`data_dir` 解析顺序 `$ZHIVE_DATA_DIR` → `$XDG_DATA_HOME/zhive` → `$HOME/.local/share/zhive`），可推导，不冗余存
- `cli_version: String` —— zhive 由 SystemNotice item 记录
- `agent_nickname / agent_role` —— codex AgentControl 私有
- `git_info: Option<GitInfo>` —— Phase 1 不收 git 元数据，留到 Phase 2 hook
- `thread_source: Option<ThreadSource>` vs `source: SessionSource` 两套 —— zhive 合成一个 `source: ThreadSource`

### Turn

| zhive 字段 | 对齐源 | 备注 |
|---|---|---|
| `id: TurnId` | codex `Turn.id`（`thread_data.rs:154`） | `turn:<thread_id>/<seq>` 形态 |
| `items: Vec<Item>` | codex `Turn.items`（`thread_data.rs:156`） | 顺序追加，不可变 |
| `items_view: TurnItemsView` | codex `items_view`（`thread_data.rs:159`） | `NotLoaded / Summary / Full` 三态（`thread_data.rs:174-185`）|
| `status: TurnStatus` | codex `status`（`turn.rs:25-33`） | `Completed / Interrupted / Failed / InProgress` |
| `error: Option<TurnError>` | codex `error`（`thread_data.rs:162` + `TurnError` `thread_data.rs:187-196`） | 仅在 `status = Failed` 时填；含 `thiserror::Error` 派生 |
| `started_at: Option<i64>` | codex `started_at`（`thread_data.rs:165`） | unix ts；`events/turn_started` 落地点 |
| `completed_at: Option<i64>` | codex `completed_at`（`thread_data.rs:168`） | unix ts；`events/turn_completed` 落地点 |
| `duration_ms: Option<i64>` | codex `duration_ms`（`thread_data.rs:171`） | `completed_at - started_at`（毫秒） |

### Item（关键字段）

| zhive `Item::*` case | 对齐源 | 备注 |
|---|---|---|
| `UserMessage { id, content }` | codex `ThreadItem::UserMessage`（`item.rs:213-215`） + ACP `SessionUpdate::UserMessageChunk`（`client.rs:86`） | `content: Vec<ItemContent>`（见下 ItemContent 表） |
| `AgentMessage { id, text }` | codex `ThreadItem::AgentMessage`（`item.rs:224-231`） + ACP `SessionUpdate::AgentMessageChunk` | 注：codex 有 `phase / memory_citation` 字段，zhive Phase 1 砍 |
| `AgentThought { id, text }` | codex `ThreadItem::Reasoning`（`item.rs:239-245`） 的 `content` 子段 + ACP `SessionUpdate::AgentThoughtChunk` | zhive 把 codex `Reasoning.content` 拍平到 `AgentThought.text`；codex `Reasoning.summary` 拍到 `Reasoning::summary` |
| `Reasoning { id, summary: Vec<String> }` | codex `ThreadItem::Reasoning`（`item.rs:239-245`） | summary 是结构化推理摘要 |
| `ToolCall { id, name, kind, status, arguments, content, locations, raw_input, raw_output }` | ACP `ToolCall`（`tool_call.rs:22-59`） 字段集 + codex `ThreadItem::McpToolCall / DynamicToolCall` | 字段名对齐 ACP：`kind: ToolKind`（10 case，`tool_call.rs:393-416`）、`status: ToolCallStatus`（4 case，`tool_call.rs:433-444`）、`content: Vec<ItemToolCallContent>`（对齐 ACP `ToolCallContent` 3 case） |
| `CommandExecution { id, command, cwd, status, exit_code, aggregated_output, duration_ms }` | codex `ThreadItem::CommandExecution`（`item.rs:248-270`） | zhive 砍 codex 的 `process_id / source / command_actions`（前者运行时状态、后者解析器输出，可由 hook 补） |
| `FileEdit { id, changes: Vec<FileUpdateChange>, status: PatchApplyStatus }` | codex `ThreadItem::FileChange`（`item.rs:273-277`） | 字段直抄 |
| `Diff { id, path, old_text, new_text }` | ACP `Diff`（`tool_call.rs:572-590`） | 独立 item 承载位（D-006 「Item::Diff」） |
| `Terminal { id, terminal_id }` | ACP `Terminal`（`tool_call.rs:531-544`） | D-006 「Item::Terminal」 |
| `Plan { id, steps: Vec<TurnPlanStep> }` | codex `TurnPlanStep`（`turn.rs:399-411`） + ACP `SessionUpdate::Plan` | 计划项；`status: TurnPlanStepStatus = Pending/InProgress/Completed` |
| `AvailableCommands { id, commands: Vec<AvailableCommand> }` | ACP `SessionUpdate::AvailableCommandsUpdate`（`client.rs:99`） | bridge 必映射 |
| `ModeChange { id, mode_id }` | ACP `SessionUpdate::CurrentModeUpdate`（`client.rs:103`） | bridge 必映射 |
| `ContextCompaction { id }` | codex `ThreadItem::ContextCompaction`（`item.rs:362`） | A4 `PreCompact` hook 触发 |
| `SystemNotice { id, level, message }` | zhive 自有 | D-012 `Notification` hook 落点 |

### ItemContent（与 ACP `ContentBlock` 1:1 同构）

| zhive `ItemContent::*` | ACP `ContentBlock`（`content.rs:36-60`） | MCP `RawContent`（`content.rs:153-159`） | 备注 |
|---|---|---|---|
| `Text { text, annotations? }` | `Text(TextContent)` | `Text(RawTextContent)` | 1:1 同构（R2 已实测） |
| `Image { data, mime_type, uri? }` | `Image(ImageContent)` | `Image(RawImageContent)` | 1:1 同构 |
| `Audio { data, mime_type }` | `Audio(AudioContent)` | `Audio(RawAudioContent)` | 1:1 同构 |
| `ResourceLink { uri, name?, description?, mime_type? }` | `ResourceLink(ResourceLink)` | `ResourceLink(RawResource)` | 1:1 同构 |
| `Resource { resource }` | `Resource(EmbeddedResource)` | `Resource(RawEmbeddedResource)` | 1:1 同构 |

### ItemToolCallContent（与 ACP `ToolCallContent` 1:1 同构）

| zhive `ItemToolCallContent::*` | ACP `ToolCallContent`（`tool_call.rs:463-474`） |
|---|---|
| `Content { content: ItemContent }` | `Content(Content)` |
| `Diff { diff: ItemDiff }` | `Diff(Diff)` |
| `Terminal { terminal_id }` | `Terminal(Terminal)` |

---

## 4. ACP `SessionUpdate` 10 case → zhive Item 映射表

> 锚点：agent-client-protocol-schema-0.12.0/src/client.rs:75-115（`#[non_exhaustive]` enum，feature `unstable_session_usage` 还会加 `UsageUpdate`）

| # | ACP `SessionUpdate` case | 子类型 | → zhive Item | 备注 |
|---|---|---|---|---|
| 1 | `UserMessageChunk` | `ContentChunk` (`client.rs:345`) | `Item::UserMessage { id, content: vec![chunk.into()] }` 追加合并 | bridge 侧需聚合：同一 message_id 的多 chunk 合并为单 Item |
| 2 | `AgentMessageChunk` | `ContentChunk` | `Item::AgentMessage { id, text: chunk.content.as_text()? }` 追加合并 | 同上 |
| 3 | `AgentThoughtChunk` | `ContentChunk` | `Item::AgentThought { id, text: chunk.content.as_text()? }` 追加合并 | 同上 |
| 4 | `ToolCall` | `ToolCall` (`tool_call.rs:27`) | `Item::ToolCall { id: tool_call_id, name: title, kind, status, content, locations, raw_input, raw_output }` | 1:1 字段映射，bridge 只填 zhive 缺的字段 |
| 5 | `ToolCallUpdate` | `ToolCallUpdate` (`tool_call.rs:169`) | **mutate 已有的 `Item::ToolCall`**（按 `tool_call_id` 查找） | `ToolCallUpdate.fields` 是部分更新；zhive 内部用 `state.update_item()` |
| 6 | `Plan` | `Plan` (acp `plan.rs`) | `Item::Plan { id, steps }` | codex 也有同名概念，字段 `step / status` 对齐 |
| 7 | `AvailableCommandsUpdate` | `AvailableCommandsUpdate` (`client.rs:413`) | `Item::AvailableCommands { id, commands }` | bridge 直转 |
| 8 | `CurrentModeUpdate` | `CurrentModeUpdate` (`client.rs:124`) | `Item::ModeChange { id, mode_id: current_mode_id }` | |
| 9 | `ConfigOptionUpdate` | `ConfigOptionUpdate` (`client.rs:163`) | `Item::SystemNotice { id, level: Info, message: "config_option: ..." }` | zhive Phase 1 不暴露独立 ConfigChange item；降级到通知 |
| 10 | `SessionInfoUpdate` | `SessionInfoUpdate` (`client.rs:205`) | **不产生 Item**，bridge 直接 mutate `Thread.name / Thread.updated_at` | 元数据更新，非内容性 |
| (feat) | `UsageUpdate` *(unstable_session_usage)* | `UsageUpdate` | **不产生 Item**，记 metric（B9 tracing）| 不进 Phase 1 wire schema |

> TODO(开放项 OP-2)：ACP `SessionUpdate` 是 `#[non_exhaustive]`（`client.rs:83`），bridge 反序列化时遇未知 case 必须降级到 `Item::SystemNotice { level: Warn, message: "unknown SessionUpdate: ..." }`，不能 panic。此约束需在 A4 + B5 落地。

---

## 5. MCP `RawContent` 5 case → zhive Item 映射表

> 锚点：${RMCP}/crates/rmcp/src/model/content.rs:149-160（`#[expect(clippy::exhaustive_enums)]`，意图穷举，但 D-006 仍按 `#[non_exhaustive]` 处理以防 1.7→1.8 升）

| # | MCP `RawContent` case | 子类型 | → zhive ItemContent | 备注 |
|---|---|---|---|---|
| 1 | `Text` | `RawTextContent { text, meta }` | `ItemContent::Text { text }` | 1:1 |
| 2 | `Image` | `RawImageContent { data, mime_type, meta }` | `ItemContent::Image { data, mime_type, uri: None }` | MCP 无 `uri` 字段 |
| 3 | `Audio` | `RawAudioContent { data, mime_type, meta }` | `ItemContent::Audio { data, mime_type }` | 1:1 |
| 4 | `Resource` | `RawEmbeddedResource { resource, meta }` | `ItemContent::Resource { resource }` | `ResourceContents { uri, mime_type, text, meta }` 二级结构对齐 ACP `EmbeddedResource.resource` |
| 5 | `ResourceLink` | `RawResource { ... }` | `ItemContent::ResourceLink { uri, name, description, mime_type }` | MCP/ACP 字段集略有差异；bridge 侧补 `name = uri.last_segment()` 即可 |

**关键转换点**：MCP `tools/call` 返回 `CallToolResult { content: Vec<Content>, structured_content, is_error }`（`content.rs` 同文件）。bridge-mcp 把整个 `CallToolResult` 合成为：

```
Item::ToolCall {
    id: tool_use_id,
    name: "<mcp_server_name>.<tool_name>",
    kind: ToolKind::Other,   // MCP 不暴露 ToolKind
    status: if is_error.unwrap_or(false) { Failed } else { Completed },
    content: result.content.iter().map(|c| ItemToolCallContent::Content { content: c.raw.into() }).collect(),
    raw_output: structured_content,
    ...
}
```

且**整个 `tools/call` 序列被 bridge 合成为一个 Turn**（D-006 §依据）：`tools/call` 进 → `events/turn_started`；`CallToolResult` 出 → `events/turn_completed`。

> TODO(开放项 OP-3)：MCP 单次 `tools/call` 合成一个 Turn，多次连续 call 是新 Turn 还是 append 到同一 Thread？建议「新 Turn / 同 Thread」（与 ACP `session/prompt` 多次发同 Session 行为一致）。

---

## 6. Rust 类型草图（cargo check 友好）

> 写在本 deliverable 内部代码块，**不进 `crates/`**（按硬约束）。所有 `todo!()` 占位。

```rust
//! Phase 1 草图：zhive-proto::domain
//!
//! D-006 落地。Thread / Turn / Item 三层原语，单 schema 源 = serde + schemars。

#![forbid(unsafe_code)]

use std::sync::Arc;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

// ============================================================
// IDs（参考 ACP `SessionId(pub Arc<str>)` 模式 + uri-style provenance）
// ============================================================

/// `thread:<provenance>/<uuid-v7>` 形态；详见 deliverable §2.2。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ThreadId(pub Arc<str>);

/// `turn:<thread_id>/<seq>` 形态。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TurnId(pub Arc<str>);

/// `item:<turn_id>/<seq>` 形态。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub struct ItemId(pub Arc<str>);

/// 对应 ACP `SessionId`（`agent-client-protocol-schema-0.12.0/src/lib.rs:99-110`）的 wire 形态。
/// zhive 内部不直接用这个 id，仅在 bridge 桥接表里持有。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AcpSessionId(pub Arc<str>);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Native,
    Acp,
    Mcp,
}

// ============================================================
// Thread
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AcpSessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ThreadId>,
    pub preview: String,
    pub ephemeral: bool,
    pub model_provider: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: ThreadStatus,
    pub cwd: std::path::PathBuf,
    pub source: ThreadSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum ThreadStatus {
    NotLoaded,
    Idle,
    Active { active_flags: Vec<ThreadActiveFlag> },
    SystemError,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ThreadActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThreadSource {
    User,
    Subagent,
    MemoryConsolidation,
}

// ============================================================
// Turn
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: TurnId,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub items_view: TurnItemsView,
    pub status: TurnStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    #[default]
    Full,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct TurnError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_details: Option<String>,
}

// ============================================================
// Item（14 case；discriminator = "itemKind"；snake_case）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "itemKind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Item {
    UserMessage {
        id: ItemId,
        content: Vec<ItemContent>,
    },
    AgentMessage {
        id: ItemId,
        text: String,
    },
    AgentThought {
        id: ItemId,
        text: String,
    },
    Reasoning {
        id: ItemId,
        #[serde(default)]
        summary: Vec<String>,
    },
    ToolCall {
        id: ItemId,
        name: String,
        #[serde(default)]
        kind: ToolKind,
        #[serde(default)]
        status: ToolCallStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ItemToolCallContent>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<ToolCallLocation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_input: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_output: Option<serde_json::Value>,
    },
    CommandExecution {
        id: ItemId,
        command: String,
        cwd: std::path::PathBuf,
        status: CommandExecutionStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        aggregated_output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },
    FileEdit {
        id: ItemId,
        changes: Vec<FileUpdateChange>,
        status: PatchApplyStatus,
    },
    Diff {
        id: ItemId,
        path: std::path::PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
    Terminal {
        id: ItemId,
        terminal_id: Arc<str>,
    },
    Plan {
        id: ItemId,
        steps: Vec<PlanStep>,
    },
    AvailableCommands {
        id: ItemId,
        commands: Vec<AvailableCommand>,
    },
    ModeChange {
        id: ItemId,
        mode_id: Arc<str>,
    },
    ContextCompaction {
        id: ItemId,
    },
    SystemNotice {
        id: ItemId,
        level: NoticeLevel,
        message: String,
    },
}

impl Item {
    /// 公共 id 访问器（参考 codex `ThreadItem::id()` `item.rs:373-394`）。
    pub fn id(&self) -> &ItemId {
        match self {
            Self::UserMessage { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::AgentThought { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileEdit { id, .. }
            | Self::Diff { id, .. }
            | Self::Terminal { id, .. }
            | Self::Plan { id, .. }
            | Self::AvailableCommands { id, .. }
            | Self::ModeChange { id, .. }
            | Self::ContextCompaction { id }
            | Self::SystemNotice { id, .. } => id,
        }
    }
}

// ============================================================
// 叶子类型（与 ACP / MCP 1:1）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemContent {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<serde_json::Value>,
    },
    Image {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Resource {
        resource: serde_json::Value, // 二级结构推到 §4 表 case #4 / B3
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemToolCallContent {
    Content { content: ItemContent },
    Diff { path: std::path::PathBuf, old_text: Option<String>, new_text: String },
    Terminal { terminal_id: Arc<str> },
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    pub path: std::path::PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PatchApplyStatus {
    Pending,
    Applied,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    pub path: std::path::PathBuf,
    pub kind: PatchChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PatchChangeKind {
    Create,
    Update,
    Delete,
    Rename,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

// ============================================================
// Turn lifecycle notification payloads（对齐 codex turn.rs:350-373）
// 这两个 notification method 名：
//   - "events/turn_started"     params = TurnStartedNotification
//   - "events/turn_completed"   params = TurnCompletedNotification
// （由 server 通过 zhive-proto::Notification 发出）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    pub thread_id: ThreadId,
    pub turn: Turn,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: ThreadId,
    pub turn: Turn,
}

// ============================================================
// Bridge 桥接表（D-006 「桥接表 + ID 命名空间」字面落地；实际表 DDL 推到 B3）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBridgeBinding {
    pub thread_id: ThreadId,
    pub bridge_session_id: AcpSessionId,
    pub bridge_kind: BridgeKind,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BridgeKind {
    Acp,
    McpSynthesized,
}
```

**编译性约束**：
- 所有 `#[non_exhaustive]` 兼容反向序列化（unknown variants 在 D-012 hook 侧给降级路径，A4 deliverable）
- 全 enum 用 `#[serde(rename_all = "snake_case")]` 或 `"camelCase"`（按 wire 决定），与 `#[derive(JsonSchema)]` 兼容
- `Arc<str>` 用于 ID 类型（对齐 ACP `SessionId(pub Arc<str>)`，`lib.rs:103`），克隆免 alloc
- `thiserror::Error` 用于 `TurnError`（对齐 codex `TurnError` `thread_data.rs:190`）

> TODO(开放项 OP-4)：草图 `Item::Resource { resource: serde_json::Value }` 把 ACP `EmbeddedResource.resource: ResourceContents`（5 字段二级结构）暂时偷懒成 `Value`。Phase 1 实现时要补成 `ResourceContents { uri, mime_type, text?, blob?, _meta? }` 强类型。这条不阻塞 §6 草图通过 `cargo check`。

---

## 7. 设计选择小结（与 §2 三大问题答案合并复述）

1. **Item enum 全集 = 14 case**（D-006 列 8 + ACP/codex 补 6）；字段命名 = codex 骨架 + ACP 叶子（含 `ItemContent / ItemToolCallContent` 1:1 同构 5/3 case）；discriminator 选 `itemKind`（避开 ACP `sessionUpdate` 与 codex/rmcp `type`）
2. **ThreadId namespace = `thread:<provenance>/<uuid-v7>`** + 桥接表（`ThreadBridgeBinding`） —— **不 1:1**，多 ACP session / 多 MCP 合成 session 可挂同一 thread
3. **Turn 边界 = JSON-RPC notification `events/turn_started` / `events/turn_completed`** + bridge 侧把 `tools/call` 序列合成 Turn（MCP）/ 把 `session/prompt` 起止合成 Turn（ACP）

---

## 8. 未决项（回流到 plan §9）

> TODO(开放项 OP-1)：D-006 决策列 8 case，本调研推荐 14 case。建议 plan §10 回流时把 D-006 字面 case 数扩到 14（含 UserMessage / Plan / AvailableCommands / ModeChange / ContextCompaction / SystemNotice 6 个新增；其中 ModeChange / AvailableCommands 是 ACP wire 直接对齐成本最低点）。
>
> TODO(开放项 OP-2)：ACP `SessionUpdate` 是 `#[non_exhaustive]`，未知 case 在 bridge 侧的降级策略需要在 A4 + B5 deliverable 中具体落地（用 `Item::SystemNotice` 还是丢弃？建议前者）。
>
> TODO(开放项 OP-3)：MCP 一次 `tools/call` 合成一个 Turn 是确定的，但**多次连续 call** 是新 Turn / append 到同一 Thread。建议「新 Turn / 同 Thread」（与 ACP `session/prompt` 多发同 Session 行为一致），但与 D-006「Turn = 一次用户输入 + 全部 agent 响应」字面有出入。需 §10 确认。
>
> TODO(开放项 OP-4)：草图 §6 中 `Item::Resource { resource: Value }` 类型暂时占位；落地时补 `ResourceContents` 强类型（5 字段，二级结构对齐 ACP `EmbeddedResource.resource`）。不阻塞编译。
>
> TODO(开放项 OP-6)：草图 `Reasoning.summary: Vec<String>` 与 `AgentThought.text: String` 是两个 item case，对齐 codex `Reasoning { summary, content }` 拆分。但 ACP 只有 `AgentThoughtChunk` 一个对应概念 —— bridge 侧从 ACP 入只能填 `AgentThought`，不能填 `Reasoning.summary`。建议把 `Reasoning` 标为 "codex-only" 入口；ACP/MCP bridge 仅写 `AgentThought`。
