# zhive TODO — 能力差距与待实现项

本文件记录「本就未实现」的能力（按用户分界线：已有功能的缺口直接优化，本就没有的能力记此处）。
对照参照为三个开源编码 agent 的**权威源码**（2026-06-04 核对）：

- **codex** — OpenAI Codex CLI（`github.com/openai/codex`，codex-rs）。28 个内置工具，能力最全。
- **opencode** — `github.com/sst/opencode`（TypeScript）。15 个内置工具。
- **pi** — `github.com/earendil-works/pi`。极简 7 工具（read/bash/edit/write/grep/find/ls），印证「在精不在多」。

zhive 现状：内置 7 工具（`read` `write` `edit` `grep` `glob` `bash` `agent`）+ skills 折叠进 system prompt。
工具**数量**已属精简合理（与 pi 同级）；本轮已直接打磨工具**质量**（grep 路径 glob、tmp 命名、SkillTool 递归、描述扩写、config 漂移守卫）。下列为净新增能力。


### 1. todo / plan 工具 — P0 / M
- **现状**：无。`grep -ri "TodoTool|PlanTool"` 全空。
- **参照**：codex `update_plan`（plan 数组 + step 状态 pending/in_progress/completed，至多一个 in_progress）；opencode `todowrite`（session 级 todo 列表）；Claude Code `TodoWrite`。
- **价值**：长任务中维护结构化进度、防止遗忘子步骤。对编码 agent 体验影响最大。
- **落地**：新增 `TodoTool`（read/write 或单 write 全量替换），持久化到 thread 级 JSON（`~/.local/share/zhive/threads/<id>/todo.json`）或纯 session 内存。无新依赖。

### 2. web_fetch / web_search 工具 — P1 / M
- **现状**：无。`tools/builtin/` 下无 web/http。当前只能 `bash + curl` 间接（且 bash `env_clear` 会丢 `HTTPS_PROXY` 等）。
- **参照**：codex `web_search`（hosted，Responses API）；opencode `webfetch`（HTTP GET→markdown）+ `websearch`（Exa/Parallel）；Claude Code `WebFetch`/`WebSearch`。
- **落地**：`WebFetchTool`（异步 GET，HTML→text/markdown，max_bytes 截断 + timeout）；可选 `WebSearchTool`（搜索 API）。
- **依赖警告**：需要 `reqwest`（HTTP 客户端）——按 CLAUDE.md「禁止新增 dependency，需先说明理由并等确认」。`zhive-mcp` 已间接经 rmcp 用 reqwest，可评估是否复用/提升为直接依赖。

### 3. multiedit / apply_patch（多文件多位置批量编辑）— P1 / M
- **现状**：`edit` 一次只改单文件单处（或全部相同串）。
- **参照**：codex `apply_patch`（自定义 freeform patch，Lark 文法，支持 Add/Update/Delete/Move 多文件）；opencode `apply_patch`（unified-diff）；Claude Code `MultiEdit`（单文件多 old/new 块）。
- **价值**：大重构时一次 tool_call 修改多处，减少往返。
- **落地**：`MultiEditTool`，参数 `edits: [{path, old_string, new_string, replace_all}]`，复用现有 `atomic_write`（write.rs）批量执行，全部成功才提交。纯 core 内、无新依赖。

### 4. read 支持图片 / PDF（multimodal tool result）— P2 / L
- **现状**：`read.rs` 明确拒绝非 UTF-8（含图片）；`ToolOutput`（tools.rs:74-82）只有 `text` + `value`，无 image content-block。`tool_dispatch/mod.rs` 固定映射为 `ItemContent::Text`。
- **参照**：codex `view_image`（本地图片→data URL，detail high/original）；opencode `read`（支持 JPEG/PNG/GIF/WebP）；pi（Ctrl+V 粘贴图片）。
- **落地（跨层）**：① `ToolOutput` 增加 `content_blocks` 支持 ImageContent；② tool_dispatch 映射支持 image block；③ provider prompt 重建支持 image tool_result。优先级取决于图片需求。

### 5. grep 上下文行（-A / -B / -C）— P2 / M
- **现状**：`grep_walk`（search.rs）每次匹配只返回命中行。
- **参照**：ripgrep / codex grep 均支持上下文行（理解函数/结构边界关键）。
- **落地**：schema 增加 `before_context` / `after_context`（默认 0），按行号范围收集 + 去重输出。
- **备注**：本轮按 advisor 评估为「给已有工具加新行为，偏净新增」而推迟；非已有功能 bug。

### 6. glob 尊重 .gitignore — P2 / M（行为变更，风险项）
- **现状**：`glob_expand`（search.rs）用裸 `glob::glob()`，**不**经 `ignore::WalkBuilder`，会返回 `target/`、`node_modules/` 等构建产物（grep 已用 WalkBuilder + git_ignore，两者不一致）。
- **落地**：改用 `ignore::WalkBuilder` 枚举 + `glob::Pattern` 过滤。
- **风险**：现 `glob_expand` 返回 `base.join(pattern)` 的**完整路径**，重写时不得无意改成相对路径（给模型的路径格式契约）。advisor 评为本批最高风险，故推迟。

---

## 二、MCP（客户端高级能力）

zhive-mcp 是基于官方 rmcp SDK 的**扎实的 tool/resource/prompt 消费客户端**：stdio + Streamable-HTTP 双传输、并行连接、超时/取消/优雅关闭、method-not-found 容错均完整。缺口集中在「客户端作为服务端回调」与「动态刷新」：

### 7. 把 MCP resources / prompts 暴露给模型与用户 — P0 / M
- **现状（真缺口）**：`McpManager` 已发现 resources/prompts 且有 `read_resource`/`get_prompt`，但 `boot.rs:338` **只消费 `manager.tools()`**——resources/prompts 发现了却无任何消费方（对用户/模型是死代码）。
- **参照**：codex `list_mcp_resources` / `list_mcp_resource_templates` / `read_mcp_resource` 作为模型工具；prompts 常作 slash 命令。
- **落地**：将 resources 暴露为模型工具（list/read），将 prompts 暴露为 slash 命令（zhive 已有 slash 框架）。

### 8. 自定义 ClientHandler（roots + logging + list_changed 动态刷新）— P1 / M
- **现状**：用 `()` no-op ClientHandler（manager.rs:41），丢弃所有 server-push 通知。
  - **roots**：`list_roots` 返回空 → MCP 文件系统服务器无法获知工作目录边界。
  - **logging**：`on_logging_message` no-op → server 日志静默丢弃。
  - **list_changed**：`on_tool/resource/prompt_list_changed` no-op → 目录仅连接时发现一次，server 动态增删工具不刷新。
- **落地**：实现 `ZhiveClientHandler`，覆写 `list_roots`（暴露 workdir）、`on_logging_message`（→ tracing）、`on_*_list_changed`（重跑 discovery + 原子更新，需 `Arc<RwLock<ConnectedServer>>`）。替换 `type Client` 的 handler 类型，改动局限在 manager.rs。

### 9. MCP sampling（server 反向请求 LLM 补全）— P2 / L
- **现状**：`()` 的 `create_message` 返回 method_not_found(-32601)。
- **落地**：自定义 handler 的 `create_message` 把请求转给引擎的 LLM provider 并等待补全（跨 crate，需访问 session/provider）。

### 10. MCP elicitation（server 向用户请求输入）— P2 / L
- **现状**：`()` 的 `create_elicitation` 自动 Decline。
- **落地**：handler 把 elicitation 转给 TUI/CLI 输入层并等待用户响应（需 MCP 层回到 UI 的通道）。

### 11. MCP 其他 — P2 / S~M
- **resource subscribe/unsubscribe + on_resource_updated**：未用 `peer.subscribe()`；资源更新不失效缓存。
- **resource templates**：未调用 `list_all_resource_templates()`。
- **completion**（`complete_prompt_argument` / `complete_resource_argument`）：未暴露；待有补全 UI 再做。
- **reconnection**：传输断开（子进程崩溃/HTTP 断连）后无重连，后续调用一律失败。可在 `McpTool::execute` 的 `TransportClosed` 上重连重试一次。
- **能力协商**：可读 `client.peer_info()` 按 server 声明的能力 gate 可选 RPC，而非全靠运行时 method-not-found。
- **legacy SSE 传输**：rmcp 1.7 已弃用双端点 HTTP+SSE（改 Streamable-HTTP）。属 SDK 方向决定的**有意不支持**；如需对接旧 server 走代理/桥接。建议在 `McpTransport` doc 注明。

---

## 三、核心开发能力（codex / opencode 已有，zhive 未实现）

### 12. LSP 集成 — P1 / L
- **参照**：opencode 在 `edit`/`write` 后收集并回传 LSP 诊断；提供 go-to-def / find-refs / hover / rename。
- **价值**：编辑后即时诊断、跨文件符号导航，对编码质量提升显著。

### 13. 沙箱后端实现（Landlock / Seatbelt / Seccomp）— P1 / L
- **现状**：zhive 已有 `Sandbox` trait seam + `DefaultSandbox`（no-op），但**无真实后端**（builtin.rs）。bash 已做 process-group 隔离 + env 收紧，但无文件系统/网络约束。
- **参照**：codex 三档 sandbox_mode（danger-full-access / workspace-write / read-only）+ 网络 seccomp + per-command 权限升级（`sandbox_permissions` + `justification`）。
- **落地**：实现 `LandlockSandbox`(Linux) / `SeatbeltSandbox`(macOS) 填充现有 seam。

### 14. 审批策略细化 — P1 / M
- **现状**：zhive 有 permission 系统（permission.rs + handshake）。
- **参照**：codex 五档 `AskForApproval`（never / on-failure / on-request / unless-trusted / untrusted）+ 可复用 `prefix_rule` 审批前缀。
- **落地**：评估 zhive permission 与 codex 策略集的差距，补齐缺失档位与「记住此前缀」。（部分已有→需先审计对齐再定直接修 vs todo。）

### 15. config profiles（命名配置档）— P1 / M
- **现状**：zhive config 支持多个命名 provider（`[provider.<name>]`），但非 codex 式 profile。
- **参照**：codex `ConfigProfile`——每个 profile 可整体覆盖 model + approval + sandbox + 其他，一键切换。
- **落地**：`[profile.<name>]` 段，CLI `--profile` 选择。

### 16. goals 系统（持久化、预算化的长任务目标）— P2 / L
- **参照**：codex `get_goal` / `create_goal` / `update_goal`，带 token 预算、状态、跨 turn 的 blocked 审计。
- **价值**：自治长任务的预算与目标追踪。

### 17. deferred tool loading（tool_search，BM25 懒加载工具）— P2 / M
- **参照**：codex `tool_search`——工具可标记 `defer_loading`，按需用 BM25 检索后再暴露，控制工具数膨胀对 context 的压力。
- **价值**：当 zhive 工具/ MCP 工具数量增长后才需要；现阶段 7 工具无压力，记录备用。

### 18. memories（agent 长期记忆工具）— P2 / M
- **现状（部分）**：zhive 已有持久化层 `persistence/memories_db.rs`（upsert + search），但**未暴露为模型工具**。
- **参照**：codex `memories` crate（read/write memory entries）。
- **落地**：在现有 memories_db 之上加 `MemoryTool`（read/write/search）暴露给模型。

### 19. code review 模式 — P2 / M
- **参照**：codex 专用 review session（禁用 web_search/view_image/goal 工具的受限模式）。

### 20. plan 模式 / background subagents — P2 / M
- **参照**：opencode plan mode（`plan_enter`/`plan_exit` 切换只读规划态）；background subagents（`task` 工具 `background:true` 异步跑）。
- **现状**：zhive 有 `agent` 工具（同步子 agent），无规划态、无后台异步子 agent。

### 21. image generation — P2 / S
- **参照**：codex `image_generation`（hosted，Responses API）。低优先。

---

## 四、已有工具的质量跟进（推迟项）

### 22. bash 环境变量白名单 — P1 / S（安全敏感，需决策）
- **现状**：`apply_minimal_env`（builtin.rs:181-195）只透传 `PATH`/`HOME`/`TERM`。cargo/git/npm 实际常需 `LANG`/`USER`/`CARGO_HOME`/`RUSTUP_HOME`/`XDG_CONFIG_HOME` 等——缺失会导致 cargo 找不到工具链、git commit 作者为空。
- **为何未直接改**：窄名单是**有意的安全边界**（代码有显式注释，防 secret 泄漏）。放宽 = 拿安全换兼容，属需用户拍板或转黑名单（清除 `*_KEY`/`*_TOKEN`/`*_SECRET`/`PASSWORD*` 前缀）的设计决策。
- **可选落地**：① 谨慎扩白名单（LANG/USER/CARGO_HOME/RUSTUP_HOME）；或 ② 改黑名单 + 经 `BuiltinToolsConfig` 注入额外白名单。

### 23. bash stdout/stderr 时序交织 — P2 / M
- **现状**：`build_tool_output`（bash.rs）`combined = stdout + stderr`，stderr 全部追加在 stdout 之后，非时序交织。构建工具的错误常夹在 stdout 中间，模型易误判上下文。
- **本轮已做**：在 bash 描述中明确「stderr 追加在 stdout 之后」让模型预期正确（最小修复）。
- **完整落地**：逐行交织读取两路流（spawn + BufReader 轮询）。

---

## 附：zhive 已具备（对照参照的平价能力，无需再做）
- AGENTS.md / CLAUDE.md 项目指令加载（system_prompt.rs）
- 会话 resume / rollout 持久化、上下文 compaction
- 生命周期 hooks（hooks/）
- permission 握手系统、子 agent（`agent` 工具）
- MCP 工具消费（stdio + Streamable-HTTP）
- skills 多根发现 + 注入 + slash 执行
- 多 provider 后端、TUI 主题
