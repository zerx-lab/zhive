---
topic: zhive 各模块外部参考项目对应关系
date: 2026-05-27
status: active
---

# zhive 外部参考项目对应关系

本文件维护 **zhive 每个待开发模块 → 参考的外部项目文件/字段/PR** 的对应表，目的是在用户本地拉取参考项目代码后，开发时能快速定位"抄哪里的"。

## 使用方式

1. 按 [§ Git clone 清单](#git-clone-清单) 把参考项目拉到本地（推荐统一放 `~/work/references/`）
2. 写 zhive 某个模块前，先查本文件对应章节
3. 用 `${REF}/<repo>/<path>` 形式记录参考点；开发时直接打开对应文件
4. **本表不是抄袭目录**：抄结构 / 抄字段名 / 抄状态机，不抄实现。Phase 1 的禁新依赖红线（[CLAUDE.md](../../CLAUDE.md) + [99 § 红线](../99-decisions/README.md#红线)）仍生效

## 维护规则

- 新模块开工前补一行参考点；如果没有任何参考、纯自创，标注 `自创`
- 参考点必带：仓库 + 文件路径 / PR 编号 / 字段名（**任一可定位锚点**）
- 上游 commit 漂移导致路径失效时改 status 为 `stale` 并更新；旧链接不删（保留推导轨迹）
- 参考点的"借鉴 vs 抄字段 vs 反例"三类必须标清楚（图例见下）

图例：

- `📋 抄字段/结构`：schema 或字段命名直接对齐，便于互操作
- `🧭 借鉴架构/状态机`：抄设计思路与拆分边界，自己实现
- `⚠️ 反例`：知道为什么"不要那么做"

---

## 一、按 zhive crate 组织（D-001 确定的 7 crate）

### zhive-proto

> JSON-RPC schema (serde + schemars) + framing 工具。`zhive-core` 与所有 client / bridge 的共享地基。

#### JSON-RPC 2.0 wire format

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`openai/codex` `codex-rs/app-server-protocol/src/jsonrpc_lite.rs`](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/jsonrpc_lite.rs) | 🧭 | request/response/notification enum + dispatcher 结构。⚠️ codex 是 "lite"（不写 `jsonrpc:"2.0"` 字段），zhive 走严格 2.0 |
| [JSON-RPC 2.0 spec § 4-5](https://www.jsonrpc.org/specification) | 📋 | 字段 `jsonrpc / id / method / params / result / error` 严格对齐 |
| [`ebkalderon/tower-lsp`](https://github.com/ebkalderon/tower-lsp) | 🧭 | stdio 上跑 JSON-RPC 的现存标杆（LSP 实现），看 request/response 路由 |

#### Framing（zhive-proto::framing）

| 参考点 | 类型 | 用途 |
|---|---|---|
| LSP Content-Length header 规范（同上 tower-lsp 实现） | 📋 | length-delimited 帧格式直接照搬 |
| [`paritytech/jsonrpsee` issue #5](https://github.com/paritytech/jsonrpsee/issues/5) | ⚠️ | 确认 jsonrpsee 不支持 stdio，所以必须自写 ~200 行 framing |

#### v1/v2 命名空间 + initialize 握手

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` `codex-rs/app-server-protocol/` v1 + v2 并存 | 🧭 | method 路径前缀 `v1/...` `v2/...` 的拆法；v2 落点：thread / permissions / remote_control / realtime |
| [codex 删 v1 plumbing PRs（本周）](https://github.com/openai/codex/pulls?q=is%3Apr+v1+plumbing) | ⚠️ | 反向证明：双命名空间 + 版本化是正解。一次性删 v1 需要 6+ PR，说明命名空间隔离生效 |
| [ACP `initialize`](https://agentclientprotocol.com/protocol/schema#initialize) | 📋 | initialize 字段对齐（protocolVersion / clientCapabilities / serverCapabilities） |

#### 三层原语 Thread / Turn / Item（D-006）

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` v2 thread schema（TS 类型单源） | 🧭 | Thread / Turn 边界拆分；codex v2 不用 prost，用 TS 类型生成 schema —— zhive 用 Rust + schemars 对等模式 |
| [ACP `Session` + prompt-turn](https://agentclientprotocol.com/protocol/schema#session) | 📋 | Session 与 Turn 的对应；Thread ↔ Session 是桥接表（不是 1:1） |
| [MCP `RawContent` / ACP `ContentBlock`](https://modelcontextprotocol.io/specification/server/tools#response) | 📋 | Item 的内容类型直接对齐两者（1:1 同构，~50 行映射） |
| [ACP `ToolCallContent::Diff/Terminal`](https://agentclientprotocol.com/protocol/schema#tool-call-content) | 📋 | Item enum 新增 `Diff / Terminal / Thought` 三类的承载位 |

#### Hooks JSON schema（D-012）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [Claude Code Hooks docs](https://code.claude.com/docs/en/agent-sdk/hooks) | 📋 | 字段命名直接对齐：`{ systemMessage?, continue?, hookSpecificOutput: { permissionDecision, permissionDecisionReason, updatedInput, additionalContext, updatedToolOutput } }` |
| Claude Code 19 个事件枚举（同上） | 📋 | 取其 14 个子集；事件枚举 `PreToolUse / PostToolUse / PostToolUseFailure / UserPromptSubmit / SessionStart / SessionEnd / SubagentStart / SubagentStop / PreCompact / PermissionRequest / Stop / Notification / Setup / ToolApprovalChange` |
| Hook base 字段：`session_id / cwd / hook_event_name`；subagent 上下文 `agent_id / agent_type / parent_tool_use_id` | 📋 | Claude Code SDK 字段名 |

#### Permission schema（D-008）

| 参考点 | 类型 | 用途 |
|---|---|---|
| Claude Code SDK `PermissionDecision` | 📋 | 四态 `deny / defer / ask / allow`，reducer 合并规则 `deny > defer > ask > allow` |
| Claude Code Subagent docs | 📋 | `inherited_permissions` 字段；父 PermissionScope 必传子 / 子可缩窄不可放大 |
| Pi CLI `streamingBehavior`（地址待补，91 R3 引入） | 📋 | `StreamingBehavior: steer \| followUp` 二元 mode 进 schema |

#### Capabilities 协商

| 参考点 | 类型 | 用途 |
|---|---|---|
| codex v1/v2 capabilities flag | 📋 | `hooks / subagents / skills / permissions / extensions / streaming_behavior` 独立 flag 命名 |
| [ACP `AgentCapabilities`](https://agentclientprotocol.com/protocol/schema#agent-capabilities) | 📋 | 字段名互操作对齐 |

#### Extension manifest（D-013）

| 参考点 | 类型 | 用途 |
|---|---|---|
| Pi CLI Extension manifest 公开 schema（地址待补） | 📋 | 顶层 namespace `extension \| prompt \| skill`；`slash_command` / `hook` 作为 extension manifest 下的子 section；filesystem-discovered + model-invoked |
| [Gemini CLI extensions](https://github.com/google-gemini/gemini-cli/blob/main/docs/extensions/index.md) | 🧭 | declarative extensions manifest 含 prompts/MCP/commands/skills/hooks/sub-agents 的字段组织方式 |

#### AgentCard schema（Phase 3 占位，D-015）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [A2A spec](https://a2a-protocol.org/latest/specification) | 📋 | AgentCard 字段；HTTP+JSON 手写编码 ~50 行 |
| [`a2aproject/a2a-js`](https://github.com/a2aproject/a2a-js) | 🧭 | 看 AgentCard 在 JS 实现里的字段使用方式 |
| [`a2aproject/A2A`](https://github.com/a2aproject/A2A) `a2a-rs 0.2.0` | ⚠️ | 单维护者 + pre-1.0，**不引依赖**（D-015） |

---

### zhive-core

> 引擎 + state + persistence + hooks host + server module。Phase 1 一切核心机制的栖息地。

#### server module（JSON-RPC over UDS + stdio，D-004）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`openai/codex` `codex-rs/app-server-transport/`](https://github.com/openai/codex/tree/main/codex-rs/app-server-transport) | 🧭 | 多 transport 抽象：stdio / unix_socket / websocket / remote_control。zhive Phase 1 走前两个 |
| `ebkalderon/tower-lsp` | 🧭 | stdio JSON-RPC 的事件循环 / 取消处理标杆 |
| UDS 默认路径 `$XDG_RUNTIME_DIR/zhive.sock` 或 `/tmp/zhive-<uid>.sock`，权限 0600 | 📋 | XDG Base Directory 规范；权限模式 codex 用同样数 |

#### Session 持久化（D-011）

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` `codex-rs/.../rollout/` JSONL append-only | 🧭 | source-of-truth 文件结构；每行 JSON 时间戳排序；append-only 不可改 |
| `openai/codex` `codex-rs/.../state/` SQLite 索引 | 🧭 | 索引表设计（按 ts / parent / thread 查询） |
| [`launchbadge/sqlx`](https://github.com/launchbadge/sqlx) `0.8`（SqlitePool + 内建异步连接池） | 📋 | 版本范围；`runtime-tokio + sqlite + migrate + macros + json` features，编译期 `sqlx::migrate!` 内嵌迁移 |
| [codex PR #24591 拆 memories_1.sqlite](https://github.com/openai/codex/pull/24591) | ⚠️ | 这是 codex 演进的"中段"，**zhive 不照搬**。zhive Phase 1 直接按 domain 拆 4 库（state / logs / memories / goals），各自独立 migrations |

#### Permission reducer（D-008）

| 参考点 | 类型 | 用途 |
|---|---|---|
| Claude Code `deny > defer > ask > allow` reducer | 🧭 | 多 hook 并行 fold 的合并规则 |
| Claude Code Subagent inheritance（同 schema 章节） | 🧭 | reducer 在父子两侧各执行一次的状态机 |

#### Hook host

| 参考点 | 类型 | 用途 |
|---|---|---|
| Claude Code SDK callback chain 模型 | 🧭 | hooks 是 callback chain（不是 pub/sub）。in-process trait + JSON schema 反序列化 |
| [`cline/cline` `apps/vscode/src/core/hooks`](https://github.com/cline/cline/tree/main/apps/vscode/src/core/hooks) | ⚠️ | FS-discovered + 测试 fixture 即 spec —— 借鉴；但 ⚠️ 8 个事件不够，照 Claude Code 14+ |

#### Subagent 调度

| 参考点 | 类型 | 用途 |
|---|---|---|
| [Claude Code Subagents docs](https://code.claude.com/docs/en/agent-sdk/subagents) | 🧭 | fresh context window / only final message returns / **禁递归** —— 三条硬约束 |

#### Skills 发现器（D-013）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [Claude Code Skills docs](https://code.claude.com/docs/en/agent-sdk/skills)（2.1.3, 2026-01-24 引入） | 🧭 | filesystem-discovered + model-invoked 扫描模式 |
| 扫描路径 `<cwd>/.zhive/skills/**/SKILL.md` + `~/.zhive/skills/` + `.zhive.local/` | 📋 | `settingSources` 三层（user / project / local） |

#### 取消传播 + StreamingBehavior 状态机

| 参考点 | 类型 | 用途 |
|---|---|---|
| Pi CLI `pending extension UI requests` Map 设计 | 🧭 | StreamingBehavior::Steer 时 in-flight tool_call 撤销 / 已发 reverse-request 回收 / Turn 边界重置 |
| ACP `session/cancel` | 📋 | 取消语义对齐 |

#### tracing spans（D-014）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [Gemini CLI OTel 一等公民](https://github.com/google-gemini/gemini-cli) | 🧭 | OTLP gRPC/HTTP 双导出模式 —— 但 zhive 只把 exporter 作 feature gate |
| `tracing` 必覆盖 spans：`Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint` | 📋 | D-014 硬约束 |

#### LLM provider 抽象（Phase 1 占位）

| 参考点 | 类型 | 用途 |
|---|---|---|
| `llmsdk` crate（项目已 git 依赖） | 📋 | 项目内统一 provider 抽象 |
| Aider `litellm` | 🧭 | 统一抽象 vs 平铺的对照 |
| `cline/cline` `providers/*.ts` 平铺 | ⚠️ | 反模式：每加一个 provider 一个 PR。zhive 必须走 trait 抽象 |

---

### zhive-bridge-stdio（D-005 + D-010）

> Phase 1 必交付的 ~90 行 io::copy + ACP minimal conformance harness。

#### ACP minimal harness

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`agentclientprotocol/rust-sdk`](https://github.com/agentclientprotocol/rust-sdk) `0.13`（caret） | 📋 | ACP runtime；仅在本 crate 引用 |
| [ACP spec § session](https://agentclientprotocol.com/protocol/schema#session) | 📋 | 验收集：`initialize / session/new / session/prompt / session/update / session/cancel` |
| ACP `SessionUpdate` 10 个 case 映射 | 📋 | `AcpAdapter` trait 自封；R2 实测 ~300-400 行 |
| 验收 harness 位置：`crates/zhive-bridge-stdio/tests/acp_conformance.rs` | 📋 | D-010 硬约束（不放 xtask） |

#### MCP 映射（Phase 2，zhive-bridge-mcp 才落地）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`modelcontextprotocol/rust-sdk`](https://github.com/modelcontextprotocol/rust-sdk) `rmcp 1.6`（caret） | 📋 | 版本范围；`#[non_exhaustive]` 已就位 |
| rmcp → zhive Item ~150-250 行（R2 实测）；ContentBlock ↔ RawContent 1:1 同构 ~50 行 | 📋 | 工程量已量化 |
| [`docs.rs/rmcp`](https://docs.rs/rmcp) | 🧭 | transport 选项 stdio / TokioChildProcess / StreamableHttpClient/Service / SSE |
| [warpdotdev/Warp 1.6](https://github.com/warpdotdev/Warp) rmcp production 用法 | 🧭 | production 集成参考 |

---

### zhive-tui（D-002）

> 仅依赖 `zhive-client-native + zhive-proto`，不依赖 core。

| 参考点 | 类型 | 用途 |
|---|---|---|
| [codex PR #22695](https://github.com/openai/codex/pull/22695)（2026-05-14） | 🧭 | TUI 完全脱 core 的拆分动作；依赖收敛到 `app-server-client + app-server-protocol + core-plugins + core-skills` |
| `openai/codex` `codex-rs/tui/` | 🧭 | ratatui 工程化：组件树 / 事件循环 / 状态机分层 |
| [`sst/opencode`](https://github.com/sst/opencode) SolidJS + `@opentui/solid` | ⚠️ | OpenCode 把 TUI 跑在 server 进程内 —— **不是** zhive 路线，但作为"TUI 即一个客户端"的反向佐证 |

---

### zhive-client-native

> Rust client lib，未来 IDE / Web / 远程客户端共享接入点。

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` `codex-rs/app-server-client/` | 🧭 | Rust client lib 的 API 表面 / 重连 / 取消处理 |

---

### zhive-cli

> 分发器。

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` `codex-rs/cli/` | 🧭 | subcommand 分发模式；`zhive bridge-stdio` / `zhive serve` / `zhive tui` |
| [Claude Code headless `-p --input-format stream-json --output-format stream-json`](https://code.claude.com/docs/en/headless) | 📋 | Phase 2 `zhive-exec` 的 stream-json 模式字段对齐 |

---

### xtask

> 构建 / 迁移 / upstream 跟版工具。**禁止引入 `acp-rust` / `rmcp`**（D-001）。

| 参考点 | 类型 | 用途 |
|---|---|---|
| matklad ["Large Rust Workspaces"](https://matklad.github.io/2021/08/22/large-rust-workspaces.html) | 🧭 | xtask 模式起源 |
| `xtask check-upstream` 命令（R5 finding #5） | 📋 | 每月一次 diff `agent-client-protocol` / `rmcp` / `sqlx` 上游 patch |

---

## 二、模块横切的工程实践参考

### 编译速度（D-009）

| 参考点 | 类型 | 用途 |
|---|---|---|
| matklad ["Fast Rust Builds"](https://matklad.github.io/2021/09/04/fast-rust-builds.html) | 🧭 | `[profile.dev] debug = "line-tables-only"` + `opt-level = 1` for deps |
| `mold` / `lld` linker | 📋 | `.cargo/config.toml` 已 in-place |
| `sccache` rustc-wrapper | 📋 | **必须 `CARGO_INCREMENTAL=0`**（[memory feedback-sccache-incremental](../../../.claude/projects/-home-zero-Desktop-code-zerx-lab-zhive/memory/feedback-sccache-incremental.md)） |
| `openai/codex` 工作区 Cargo 配置 | 🧭 | dev profile / split-debuginfo / shared deps 配法 |

### Rust workspace 拆分演进节奏

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` 8→11 crate 用 9 天 / 8→96 用 13 月 | 🧭 | 起步 7 crate 后按需拆分的节奏证据 |
| `warpdotdev/Warp` ~60 crate | ⚠️ | 反例：起步过细 = import 折磨 |

---

## 三、Phase 3 候选参考（不进 Phase 1 / 2 核心路径）

> 这些参考点 **Phase 1 不要看**，避免抵押当前复杂度。Phase 3 评估远程 / Web / 高性能 IPC / 团队协作时再开启。

### 远程 transport / WebSocket 隧道

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` `codex-rs/app-server-transport/src/transport/remote_control/protocol.rs` | 🧭 | WebSocket 复用多客户端 JSON-RPC |
| [codex PR #24164](https://github.com/openai/codex/pull/24164) / [#23775](https://github.com/openai/codex/pull/23775) / [#24473](https://github.com/openai/codex/pull/24473) | 🧭 | remote_control 子系统的演进轨迹 |
| `openai/codex` `codex-rs/.../auth.rs` JWT + SHA256 双模 | 🧭 | 远程鉴权方案 |

### Effect HttpApi + SSE 控制平面

| 参考点 | 类型 | 用途 |
|---|---|---|
| `sst/opencode` HTTP REST + SSE via Effect HttpApi（113 paths, 103 event types） | 🧭 | Web UI 控制平面候选 |
| `sst/opencode` `/sync/start` / `/sync/replay` / `/experimental/workspace/warp` | 🧭 | multi-workspace session 迁移设计 |
| [`sst/opencode/packages/sdk/openapi.json`](https://github.com/sst/opencode/blob/dev/packages/sdk/openapi.json) | 🧭 | OpenAPI codegen → SDK 模式 |

### 高性能 IPC（如果 JSON-RPC 性能瓶颈出现）

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`warpdotdev/Warp/blob/main/crates/ipc/Cargo.toml`](https://github.com/warpdotdev/Warp/blob/main/crates/ipc/Cargo.toml) bincode 1.3 + interprocess 1.2 | 🧭 | UDS / Named Pipe 上的二进制热路径 |
| Warp protobuf 仅作 schema language（`prost-types 0.14`，不做 transport） | 🧭 | schema 与 wire 解耦的反向佐证（zhive 选 serde + schemars 走类似路） |

### Session 持久化按 domain 拆库

| 参考点 | 类型 | 用途 |
|---|---|---|
| `openai/codex` PR #24591（memories_1.sqlite 拆分） | 🧭 | 进一步按子 domain 拆库的演进先例（zhive Phase 1 已按 state / logs / memories / goals 拆 4 库，Storage trait 留口） |
| [`warpdotdev/Warp/tree/main/crates/persistence/migrations`](https://github.com/warpdotdev/Warp/tree/main/crates/persistence/migrations) | 🧭 | 时间戳迁移目录设计 |

### Repo 智能 / 长上下文压缩

| 参考点 | 类型 | 用途 |
|---|---|---|
| [`Aider-AI/aider`](https://github.com/Aider-AI/aider) `repomap.py`（tree-sitter + networkx） | 🧭 | 代码图压缩；91 明确"值得借鉴" |
| Aider `ChatSummary` 递归摘要 | 🧭 | 长上下文压缩策略 |

### A2A（Phase 3 仅作 AgentCard 占位）

| 参考点 | 类型 | 用途 |
|---|---|---|
| `google-gemini/gemini-cli` `a2a-server` package | 🧭 | A2A 服务端字段使用 |
| [A2A spec](https://a2a-protocol.org/latest/specification) | 📋 | AgentCard / Task / Message 三层模型 |
| `a2aproject/a2a-js` | 🧭 | 字段在 JS SDK 的使用方式（**不是引依赖**） |

---

## 四、协议层面的反例（明确不要做的）

> 这些项目的部分设计被多轮 review 判定为反模式，但仍有借鉴价值。开发前先看反例条目避免重蹈。

| 项目 | 反例点 | zhive 对策 |
|---|---|---|
| `cline/cline` | `providers/*.ts` 平铺（每加 provider 一个 PR） | trait 抽象 + `llmsdk` 统一接入 |
| `cline/cline` | JSON 文件存储 + `state-migrations.ts` | SQLite + Storage trait（D-011） |
| `cline/cline` | 每 tool 一个 TS handler class | 声明式 JSON-Schema 注册（D-013） |
| `cline/cline` | 仅 8 个 lifecycle hooks | 14+ 个（D-012），`#[non_exhaustive]` |
| `Aider-AI/aider` | coder × edit-format 类爆炸（10+ coder 子类） | 单一 prompt 接口 + 配置驱动 |
| `anthropics/connect-rust` `=0.6.x` | 0.4→0.5→0.6 三连 breaking minor，pre-1.0 强制 buffa codegen | D-003 推翻 ConnectRPC，Phase 1 走 JSON-RPC 2.0 |
| `a2aproject/a2a-rs` `0.2.0` | 单维护者 + pre-1.0 + 月更新可能停滞 | D-015 不引依赖，AgentCard 手写 |
| `spacejam/sled` `0.34.7` | stuck since 2021-09，作者已转 komora | D-011 拒绝；用 sqlx 0.8（SqlitePool） |
| `sst/opencode` Go TUI 下线、TUI 跑 server 内 | 与 zhive "TUI 是一个客户端" 路线相反 | D-002 TUI 脱 core，独立进程 |
| `openai/codex` PR #24591 多 SQLite 拆库 | 这是 codex 演进**中段**，不是起点 | D-011 Phase 1 按 domain 拆 4 库（state / logs / memories / goals）；Storage trait 留口 |
| `warpdotdev/Warp` ~60 crate 起步 | 起步过细 = import 折磨 | D-001 起步 7 crate |

---

## 五、Git clone 清单

建议统一放 `~/work/references/`，避免污染 zhive 工作区。

```bash
# 一次性拉取参考代码
mkdir -p ~/work/references && cd ~/work/references

# === 一等参考（Phase 1 立刻要用） ===
git clone https://github.com/openai/codex.git                              # codex-rs 在 codex-rs/ 子目录
git clone https://github.com/agentclientprotocol/rust-sdk.git acp-rust-sdk # ACP 0.13
git clone https://github.com/modelcontextprotocol/rust-sdk.git rmcp        # MCP rmcp 1.6
git clone https://github.com/ebkalderon/tower-lsp.git                      # stdio JSON-RPC 标杆
git clone https://github.com/launchbadge/sqlx.git                          # sqlx 0.8（SqlitePool）

# === 二等参考（schema 字段对齐、横向对照） ===
git clone https://github.com/sst/opencode.git                              # ACP next + HttpApi + SSE
git clone https://github.com/google-gemini/gemini-cli.git                  # MCP+ACP+A2A 三协议 + OTel
git clone https://github.com/warpdotdev/Warp.git                           # Rust workspace 反例 + IPC 热路径
git clone https://github.com/cline/cline.git                               # hooks 反例 + provider 反例
git clone https://github.com/Aider-AI/aider.git                            # repomap / ChatSummary

# === 协议规范（文档为主，无代码也建议本地放一份 PDF/clone） ===
git clone https://github.com/a2aproject/A2A.git                            # A2A spec（Phase 3）
git clone https://github.com/a2aproject/a2a-js.git                         # AgentCard JS SDK（Phase 3）

# === 反例 / 拒绝引依赖（仅看实现细节，不引） ===
git clone https://github.com/anthropics/connect-rust.git                   # 知道为什么不用
git clone https://github.com/paritytech/jsonrpsee.git                      # 确认无 stdio 支持

# === Claude Code 无 OSS 代码，仅文档 ===
# 文档站：https://code.claude.com/docs/en/agent-sdk/overview
# 建议 wget 镜像或导出 PDF：hooks / subagents / skills / headless 四篇关键页
```

### 锚点 commit（91 调研快照）

| repo | 调研日期 | 状态 |
|---|---|---|
| openai/codex | 2026-05-27 主分支 | 91 R1 快照 |
| agentclientprotocol/rust-sdk | `=0.12.1` 锁定 | D-005 |
| modelcontextprotocol/rust-sdk | `rmcp =1.7.0` 锁定 | D-005 |
| rusqlite/rusqlite | `=0.40` 锁定 | D-011 |

> 后续拉取后请用 `git log --oneline -1` 把 HEAD commit 记到本表，便于参照同一时间点。

---

## 六、待补的参考源

R3 引入但 91 未给出仓库链接的 **Pi CLI**：
- 提供了 `streamingBehavior`、`source: extension|prompt|skill`、`pending extension UI requests` Map 三个模型
- 仓库地址用户提供后补回本文 D-008 / D-013 章节，并在 [§ 五](#git-clone-清单) 加 clone 命令

---

## 引用根目录

所有一手证据已在 [91 § 一手证据汇总](../91-architecture-review-2026-05-27/README.md#一手证据汇总) 罗列；本文件不重复，仅在每条参考点旁附定位锚点（文件路径 / PR 编号 / 字段名）。
