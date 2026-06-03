# TUI 实现设计：历史会话列表 + /session resume + subagent 展示

综合三份调研（zhive-tui 机制 / zhive 后端 / opencode 交互）+ conversation.rs 精读。核心借鉴 opencode：**subagent = 带 parent 关系的独立子会话，父流里放一行"活体摘要"，可进入子会话视图**；session 列表 = 通用 fuzzy-finder overlay。

## 0. 后端 API 契约（由后端 Agent 落地，TUI 接线依赖）
- `thread/list` → `{ threads: [Thread...] }`（Thread.id/name/preview/updated_at/forked_from）
- `engine/resume_thread { threadId }` → 恢复历史 thread 使可续聊
- `thread/get_items { threadId, turnId?, offset?, limit? }` → `{ items: [Item...] }`（resume 后渲染历史）
- `events/subagent_started { parentThreadId, childThreadId, agentType?, description? }`（可能附 parentToolUseId）
- `events/subagent_completed { parentThreadId, childThreadId, hasFinalMessage }`
- 子 thread 的 `events/turn_started`/`item_appended`/`turn_completed` 已广播（带子 thread id），TUI 据 parent↔child 路由。

## 1. 数据模型（conversation.rs）
现状：`Conversation { thread_id, turns: Vec<TurnView>, busy, streaming, last_error }`，`apply` 按 turn_id fold，**不看 thread_id**。

改造：
```rust
pub struct Conversation {
    pub thread_id: ThreadId,
    pub turns: Vec<TurnView>,
    pub busy, streaming, last_error,
    pub subagents: Vec<SubagentView>,   // 新增：本会话内派生的子 agent（按 child_thread_id 唯一）
}

pub struct SubagentView {
    pub child_thread_id: ThreadId,
    pub parent_tool_use_id: Option<ItemId>, // 关联父流的 agent ToolCall item（若后端提供）
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub turns: Vec<TurnView>,           // 子会话 turns（复用 TurnView）
    pub status: SubagentStatus,         // Running | Completed | Failed
    pub tool_calls: usize,              // 活体摘要：已发起的工具调用数
    pub current_tool: Option<String>,   // 运行中：当前工具名
}
pub enum SubagentStatus { Running, Completed { has_final: bool }, Failed }
```

## 2. apply 的 thread_id 路由（关键修复）
`apply(event)` 改为先按事件 `thread_id` 分发：
- `thread_id == self.thread_id` → 走主 turns（现有逻辑）。
- `thread_id` 命中某 `SubagentView.child_thread_id` → 路由进该子会话的 turns + 更新摘要（ItemAppended 时若是 ToolCall 则 tool_calls+1/current_tool=name；TurnCompleted 清 current_tool）。
- 新增 `EngineNotification::SubagentStarted` → push 一个 `SubagentView{Running}`。
- 新增 `EngineNotification::SubagentCompleted` → 对应 SubagentView 置 Completed。
- 未知 thread_id 的事件（既非主也非已知子）→ 忽略（防御，记 last_error 可选）。

注意：`EngineNotification` 各 variant 已带 `thread_id` 字段（protocol.rs 已解析），apply 现在要真正用它。

## 3. protocol.rs：解析两个新事件
`decode` 加：
- `events/subagent_started` → `EngineNotification::SubagentStarted { parent_thread_id, child_thread_id, agent_type, description }`
- `events/subagent_completed` → `EngineNotification::SubagentCompleted { parent_thread_id, child_thread_id, has_final }`

## 4. 渲染（ui.rs）
- **父流 subagent 活体摘要行**（对应 opencode `<Task>`）：在 `transcript_lines` 渲染主 turns 时，遇到 `Item::ToolCall { name:"agent", id, .. }`，在其下缩进渲染关联的 SubagentView 摘要：
  - 运行中：`  │ <agent> task — <desc>` + `  ↳ <current_tool>` 或 `↳ N toolcalls`（spinner）
  - 完成：`  ✓ <agent> task — <desc>` + `  ↳ N toolcalls · done`
  - 关联：优先 `parent_tool_use_id == toolcall.id`；无则按 subagents 顺序/description 匹配；再无则把所有 subagent 摘要渲染在该 turn 末尾。
- ToolKind 可给 agent 工具加 SubAgent 视觉（或保持 Other + name 判断）。
- **子会话完整视图（增强，可第二步）**：route 状态 `Main | Subagent(child_thread_id)`，进入后渲染该 SubagentView.turns + 底部页脚 `Parent ↑ · Prev ← · Next →`（兄弟 = self.subagents）。MVP 先做内联摘要 + 展开/折叠（按键展开看子会话 turns）。

## 5. session 列表 + /session resume
- **通用 SelectList overlay**（对应 opencode `dialog-select.tsx`）：overlays.rs 加 `render_select_list(items, selected, query, hints)`，复用 `open_popup`+高亮逻辑（抄 render_palette）。供 session list（及未来 model list）复用。
- **slash**：app.rs `builtin_commands` 加 `("session","list / resume sessions")`（别名 resume）；`run_slash` 加 `"session"|"resume" =>` → `Action::OpenSessionList`。
- **Action/Overlay**：加 `Action::OpenSessionList`、`Action::ResumeSession{thread_id}`；`Overlay::SessionList{ entries: Vec<SessionEntry>, selected, query }`。
- **perform**（lib.rs）：`OpenSessionList` → `rpc::list_threads(client)` 填 overlay；`ResumeSession` → `app.reset_thread(thread_id)`（已有）+ `rpc::get_thread_items` 拉历史 replay 进新 Conversation（或调 `engine/resume_thread` 让后端注册 + 后续事件自然流入）。
- **on_overlay_key**：`Overlay::SessionList` 分支处理 Up/Down/Enter(resume)/Esc/输入过滤。
- **rpc.rs**：加 `list_threads(client) -> Vec<SessionEntry>`（调 `thread/list`）、`get_thread_items(client, thread_id) -> Vec<Item>`（调 `thread/get_items`）、`resume_thread(client, thread_id)`（调 `engine/resume_thread`）。
- 列表每项：标题（name/preview）+ 相对时间 + 当前会话 `●` 标记；可选分组 Today/日期。

## 6. 文件改动清单
| 文件 | 改动 |
|---|---|
| conversation.rs | SubagentView/SubagentStatus + subagents 字段 + apply thread_id 路由 + SubagentStarted/Completed 处理 |
| protocol.rs | decode events/subagent_started + subagent_completed → 新 EngineNotification variant |
| ui.rs | 父流 subagent 活体摘要行渲染（agent ToolCall 下缩进）+ 可选子会话视图 |
| overlays.rs | 通用 render_select_list + render_session_list |
| app.rs | builtin_commands +session；run_slash；Action::{OpenSessionList,ResumeSession}；Overlay::SessionList；on_overlay_key SessionList 分支；session_list selected/query state |
| lib.rs (perform) | OpenSessionList→list_threads；ResumeSession→reset_thread+get_items/resume_thread |
| rpc.rs | list_threads / get_thread_items / resume_thread |

## 7. 范围
- MVP（核心痛点）：thread_id 路由修复 + subagent 父流活体摘要行 + /session 列表 + resume。
- 增强（第二步）：子会话完整视图 + 页脚导航；session 列表预览窗格 + 分组 + 快速切换槽位；通用命令面板。
