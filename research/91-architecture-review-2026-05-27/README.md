---
topic: zhive 架构合理性与可扩展性多轮 review
date: 2026-05-27
status: active
---

# zhive 架构合理性与可扩展性 review（2026-05-27）

针对 [99-decisions](../99-decisions/) D-001~D-015 做多轮独立调研 + 反向批评 + 防御性反审视后的结论。

## 方法论

四轮 9 个独立 subagent 的交叉验证：

| 轮次 | Agent | 主题 |
|---|---|---|
| R1 | codex-rs | 协议/transport 演进（主分支 2026-05-27 快照） |
| R1 | Claude Code CLI / Agent SDK | hooks/skills/subagents/SDK |
| R1 | OpenCode (sst) | Effect HttpApi + ACP 双轨 |
| R1 | Gemini CLI | MCP+ACP+A2A 三协议 |
| R1 | Warp/Aider/Cline | IPC/持久化/hooks 横向对比 |
| R2 | ACP/A2A 规范 | 协议位阶 |
| R2 | Rust MCP/持久化 | rmcp / rusqlite 选型 |
| R2 | Hooks/Skills/Subagents | Phase 1 协议地基 |
| R2 | ConnectRPC 决策再审视 | D-003 防御性攻击 |
| R3 | 独立 critic | D-001~D-010 整体反向批评 |
| R4 | 协议指控反审视 | 推翻 critic 过度反应 |
| R4 | crate 拆分反审视 | Phase 1 应是 6 个 crate |

每条结论都标注一手证据 URL；R3 critic 的尖锐指控经 R4 防御性反审视后再筛选采纳。

## 一、同行架构实况（R1 摘要）

### codex-rs (OpenAI，2026-05-27 主分支)

- 协议仍是 **JSON-RPC "lite"**（不写 `jsonrpc:"2.0"` 字段），见 `app-server-protocol/src/jsonrpc_lite.rs`
- **v1 + v2 双命名空间并存**，v2 是新功能落点（thread/permissions/remote_control/realtime/...）
- TUI **完全脱离 core**（PR #22695, 2026-05-14），现仅依赖 `app-server-client + app-server-protocol + core-plugins + core-skills`
- 新增 `remote_control/` 子系统：通过 WebSocket 隧道复用多客户端 JSON-RPC（PR #24164/#23775/#24473）
- Transport：stdio / unix_socket / websocket / remote_control 四种，含 JWT + SHA256 双模鉴权（`auth.rs`）
- **至今未迁 ConnectRPC**
- 持久化：`rollout/` JSONL append-only + `state/` SQLite 索引

证据：<https://github.com/openai/codex/tree/main/codex-rs>

### Claude Code (Anthropic)

- 单 Node 二进制贯穿 CLI / IDE 扩展 / 桌面 / Web / Agent SDK
- SDK ⇄ CLI 协议 = **stream-json (JSONL over stdio)**，`-p --input-format stream-json --output-format stream-json`
- **19 个 hooks 事件**（PreToolUse / PostToolUse / PostToolUseFailure / PostToolBatch / UserPromptSubmit / Stop / SubagentStart / SubagentStop / PreCompact / PermissionRequest / SessionStart / SessionEnd / Notification / Setup / TeammateIdle / TaskCompleted / ConfigChange / WorktreeCreate / WorktreeRemove）
- Permission 合并规则：`deny > defer > ask > allow`
- Subagents：fresh context window + only final message returns + 禁止递归（"Subagents cannot spawn their own subagents"）
- **Skills 替代 slash commands**（Claude Code 2.1.3, 2026-01-24），filesystem-discovered + model-invoked
- MCP 双向：既是 host 也是 server；VS Code 插件 = MCP server（lockfile + 127.0.0.1 + 随机端口 + 0600 token）
- 无 protobuf / gRPC / 公开 schema；wire format 用 prose 文档描述

证据：<https://code.claude.com/docs/en/agent-sdk/overview>

### OpenCode (sst)

- **Go TUI 已下线**，新 TUI 是 SolidJS + `@opentui/solid`，跑在 server 进程内
- TUI 通过 SDK + SSE 与 server 通信（基本不算独立 transport）
- HTTP REST + SSE via **Effect HttpApi**（113 paths, 103 event types）
- **ACP 双轨**：`src/acp/` v1（对接 Zed）+ `src/acp-next/` 大重写中（5/25-5/26 十余 PR）
- Multi-workspace **control-plane** 一等公民（`/sync/start`, `/sync/replay`, `/experimental/workspace/warp` 把 session 偷到另一 workspace 跑）
- SDK 完全 OpenAPI codegen（`@hey-api/openapi-ts`），v1/v2 并存

证据：<https://github.com/sst/opencode>

### Gemini CLI (Google)

- TS / Node + React 19 + `ink` TUI
- **MCP + ACP + A2A 三协议解耦**：MCP 是工具协议、ACP 是 client↔agent stdio 协议（已与 Zed 兼容）、A2A 是 agent 间协议（Linux Foundation 系 `a2aproject`）
- 独立 `a2a-server` package 暴露 A2A 服务端
- declarative extensions：manifest 含 prompts/MCP/commands/skills/hooks/sub-agents
- **OpenTelemetry 一等公民**（trace + metrics + logs，OTLP gRPC/HTTP 双导出）
- 未强绑 Vertex AI（OAuth / API key / Vertex 三选一）

证据：<https://github.com/google-gemini/gemini-cli>

### Warp (开源 AGPL-3.0, 2026-04)

- ~60 个 Rust crate
- IPC hot path = **bincode 1.3 + interprocess 1.2**（UDS / Named Pipe），不用 tonic 不用 JSON-RPC
- Protobuf 仅作 schema language（`warp_multi_agent_api` 用 `prost-types` 0.14），不做 transport
- MCP 用 **`rmcp 1.6`**（官方 Rust SDK）
- 持久化 = **diesel 2.3 + SQLite**，带时间戳迁移目录
- Provider 抽象隐藏在 Warp 自家云后（`warp_server_client`）

证据：<https://github.com/warpdotdev/Warp>

### Cline (cline/cline)

- VS Code 扩展形态，gRPC over message bus（host ⇄ webview）
- **8 个 lifecycle hooks**（FS-discovered + 测试 fixture 即 spec）
- 每 tool 一个 TS handler class（不是声明式 JSON-Schema）
- Provider 平铺在 `providers/*.ts`（**反模式**：每加一个 provider 一个 PR）
- 存储 = JSON 文件 + `state-migrations.ts`（**反模式**：他们因此才写迁移层）

证据：<https://github.com/cline/cline>

### Aider

- Python 单进程，无协议层
- coder × edit-format 类爆炸（10+ coder 子类 × 多 prompt 文件）
- 依赖 litellm 作为 provider 抽象
- `repomap.py`（tree-sitter + networkx）+ `ChatSummary` 递归摘要——**值得借鉴**

证据：<https://github.com/Aider-AI/aider>

## 二、协议与组件选型（R2 摘要）

### ACP（Agent Client Protocol）

- 已实现 ACP 的 agent：30+ 个（Augment Code / AutoDev / Claude Agent SDK / Cline / Codex CLI / Cursor / GitHub Copilot / Goose / Junie / Kimi CLI / OpenHands / Pi / Poolside / Qwen Code / ...）
- 已实现 ACP 的 client：Zed / JetBrains / Emacs / Neovim / VS Code / Obsidian
- 官方 Rust SDK：`agent-client-protocol` 0.12.1（31 release，Apache-2.0，Zed-backed）
- Transport：JSON-RPC 2.0 over stdio（remote HTTP/WS WIP）
- 与 MCP 关系：ACP 显式复用 MCP 的 `ContentBlock` JSON

证据：<https://agentclientprotocol.com/protocol/schema>, <https://github.com/agentclientprotocol/rust-sdk>

### A2A（Agent2Agent Protocol）

- 三 transport：JSON-RPC over HTTPS / gRPC / HTTP+REST + SSE 流
- 三层模型：AgentCard / Task / Message
- 官方 SDK：Python / Go / JS / Java / .NET（**无官方 Rust**）
- 社区 Rust：`a2a-rs` 0.2.0（85 stars，单维护者，pre-1.0）

证据：<https://a2a-protocol.org/latest/specification>, <https://github.com/a2aproject/A2A>

### MCP Rust SDK

- **`rmcp` 1.7**（2026-05-13）—— modelcontextprotocol 官方，Apache-2.0/MIT 双
- 依赖图与 zhive 现有 tokio 1 + hyper 1 + axum 0.8 + serde_json 完全对齐
- Transport：stdio / `TokioChildProcess` / `StreamableHttpClient/Service` / SSE
- Warp 1.6 production 验证

证据：<https://github.com/modelcontextprotocol/rust-sdk>, <https://docs.rs/rmcp>

### Session 持久化

收敛证据：**codex / Warp / opencode 三家殊途同归到 SQLite**。

推荐：`rusqlite 0.40 + bundled` + JSONL rollout（codex 模式）。

劝退：

| 候选 | 拒绝原因 |
|---|---|
| sled | 0.34.7 stuck since 2021-09，作者已转 komora |
| diesel | 同步 + 重宏 + `DATABASE_URL` 环境变量痛点 |
| sqlx (sqlite) | 编译期 SQL 校验对 agent schema 价值低，破坏 `cargo check -p` 工作流 |
| sea-orm | 过度抽象，编译开销显著 |
| redb | KV-only，按时间/parent 查询要手写二级索引 |
| 纯 JSONL（无 DB） | Claude Code 路线；session 多了 list/filter 体验崩坏，codex 踩过坑才加 SQLite |

### ConnectRPC 决策再审视（R2 攻击 + R4 防御）

- connect-rust 0.6.1（2026-05-27），首发 2026-03-04（~3 个月），pre-1.0
- 0.4→0.5→0.6 全部 breaking
- "Anthropic 内部生产使用"只有自述，无公开产品证据
- **业内 0 个同行**把它当 core ⇄ TUI 协议（codex/Claude Code/opencode/Zed/Warp 全在 JSON-RPC 或自有协议阵营）
- 编译开销估算：当前 ConnectRPC 全家桶 ~90-140s cold build vs JSON-RPC 路线 ~35-55s（mold + sccache 空）

但 R4 反审视确认：
- "0 同行用"是诉诸大众，不构成拒绝理由
- pre-1.0 标准双重：rmcp 自己也 pre-1.0 但被推荐
- 编译开销数字是估算，未实测

**结论**（R2 阶段）：D-003 不全盘推翻，但加 feature gate（默认走 JSON-RPC 2.0 兼容路线）。

> ⚠️ **本节 R2 阶段结论已被 § 八 R3+R4 终版取代**：D-003 已彻底推翻，ConnectRPC 退为 Phase 3 候选 transport（理由：R2 完整性实测+R3+R4 critic/defender 三方独立证据三角化）。本节保留作推导轨迹。

## 三、Hooks/Skills/Subagents 协议地基（R2 摘要）

三家殊途同归到三个事实：

1. **协议契约是 JSON schema**，不是 Rust trait。Claude Code SDK 回调与 shell hook 共用同一 JSON 输出格式：`{ systemMessage?, continue?, hookSpecificOutput: { permissionDecision, permissionDecisionReason, updatedInput, additionalContext, updatedToolOutput } }`。
2. **Permission 决策合并 reducer**：`deny > defer > ask > allow`，多 hook 并行 fold。
3. **Subagent context isolation 是 agent loop 调度器的事**，不是后期插件——fresh window + parent→child 仅 prompt 字符串 + child→parent 仅 final message + 禁递归。

Phase 1 必须做的最小事项清单：

1. 定义 `HookEvent` JSON wire schema（serde），字段命名与 Claude Code 对齐
2. `PermissionDecision` 四态枚举 + Reducer
3. `SubagentContext` 类型（独立队列 + 工具继承 + 禁递归）
4. Skill 发现器：扫描 `<cwd>/.zhive/skills/**/SKILL.md` + `~/.zhive/skills/`
5. Hook host 抽象（in-process trait + JSON schema 反序列化）
6. `settingSources` 三层（user / project / local）

证据：<https://code.claude.com/docs/en/agent-sdk/hooks>, <https://code.claude.com/docs/en/agent-sdk/subagents>, <https://code.claude.com/docs/en/agent-sdk/skills>

## 四、Critic 反向批评经 R4 反审视后的最终判定

| Critic 指控 | R4 判定 | 行动 |
|---|---|---|
| D-003 ConnectRPC 是赌博，应改 JSON-RPC | ⚠️ 部分对：confirmed pre-1.0 + 0 同行用，但 critic 用诉诸大众 | 加 feature gate，不全删 |
| D-008 反向 RPC 应改事件总线 | ❌ 事实错误：Claude Code hooks 是 callback chain 不是 pub/sub | 保留 D-008，加强 schema 说明 |
| D-002+D-005 是 N×M 翻译矩阵 | ⚠️ 部分对：bridge 是 hexagonal adapter，但 schema drift 风险真实 | 保留 D-005，新增 contract test 要求 |
| 本地默认应改 stdio | ❌ 事实错误：critic 混淆 transport 与协议；stdio 不支持 daemon | 保留 D-004 UDS |
| Phase 1 不该开 12 crate | ✅ critic 正确：codex 真实演化 8→11 用 9 天，8→96 用 13 月 | **立即收敛到 6 crate** |
| 不写 bridge crate 直接用 rmcp/ACP-rust | ❌ 错：会污染 core 依赖图，违反 D-002 | rmcp/ACP-rust 放在 bridge 内引用，core 不直接依赖 |

## 五、不确定项消解（R5 / R6 实验完成）

| 不确定项 | 状态 | 结论 |
|---|---|---|
| ConnectRPC cold build 实测 90-140s？ | 已绕过 | R3+R4 推翻 D-003，Phase 1 不再编 ConnectRPC，问题消失 |
| ACP-rust ↔ zhive Item 映射成本？ | 已实测 | R2 量化为 ~300-400 行（10 个 SessionUpdate case），不存在不可调和冲突；但 `ToolCallContent::Diff/Terminal` 需扩 zhive Item |
| rmcp 1.7 ↔ zhive Item 映射成本？ | 已实测 | R2 量化为 ~150-250 行（3 case），ContentBlock 与 RawContent 1:1 同构 |

## 六、不变的根基（R3+R4 后仍站得住）

- **D-002 TUI 脱 core** 是正确战略（codex/opencode 双验证）
- **D-006 三层原语 Thread/Turn/Item** 是正确抽象（codex v2 + ACP prompt-turn 双验证）
- **D-007 initialize + v1/v2 capabilities** 是正确握手机制（codex 本周一次性删 v1 plumbing 反向证实）
- **D-008 反向 RPC** 机制保留（callback chain，不是 pub/sub），但 transport 改 JSON-RPC server-initiated request
- **D-009 编译速度组合拳** 是正确组合（仅砍 hakari）

## 七、R3 critic 反向批评 + R4 defender 反审视（2026-05-27）

R1+R2 完成后启动了 **2 个独立 critic** + **1 个独立 defender**。

### Critic A（攻 D-001~D-015 全集）出 10 条指控
### Critic B（专攻协议选型）出 4 条指控 + 给 3 个互斥方案（A=ACP-first / B=双协议并行 / C=codex 路线）
### Defender 逐条审视后判定

| 指控 | 判定 | 行动 |
|---|---|---|
| Critic A-1：D-011 应拆多 SQLite | ⚠️ | Storage trait 留口，Phase 1 单库起步——不跟 codex 演进的中段，从其起点起步 |
| Critic A-2：D-013 Skills 替代 SlashCommands 是过头 | ✅ | D-013 改：Skills 与 SlashCommand 是 Extension manifest 下两个并列 namespace（Pi 模型） |
| Critic A-3：D-012 hooks 只 8 个 | ✅ | 扩到 14+ 个 + `#[non_exhaustive]` enum |
| Critic A-4：D-005 没说怎么追 ACP breaking | ⚠️ | 精确锁版本 `=0.12.1` + `AcpAdapter` trait 隔离上游 |
| Critic A-5：D-003 应彻底删 | ✅ | **D-003 推翻**：Phase 1 走 JSON-RPC 2.0，ConnectRPC 退为 Phase 3 候选 transport |
| Critic A-6：D-001 vs D-005 字面矛盾 | ✅ | Phase 1 不引入 rmcp/ACP runtime；若 Phase 1 必含 bridge-stdio，则 bridge crate 同步进 Phase 1 |
| Critic A-7：D-006 引 prost 违反禁新依赖 | ✅ | 改 JSON Schema 单一来源（serde + schemars），删 prost/prost-build |
| Critic A-8：D-015 sled 与 a2a-rs 双标 | ⚠️ | 用同一把尺：a2a-rs 不进 Phase 1 / 2 / 3 核心路径，仅作 schema 抽象层占位 |
| Critic A-9：D-008 漏 subagent 权限继承 | ✅ | Subagent schema 加 `inherited_permissions` + 子可缩窄不可放大 |
| Critic A-10：D-014 OTel 一等 vs feature gate | ⚠️ | tracing 进 Phase 1 核心（spans 覆盖 Turn/Hook/Subagent/Permission），OTel exporter 才 feature gate |
| Critic B：D-003 实际只 Phase 3 才用，今天付编译成本 | ✅ | 同 A-5，Phase 1 协议彻底走 JSON-RPC 2.0 |
| Critic B：D-004 UDS 单选过早收敛 | ✅ | Phase 1 同时含 stdio + UDS，由 `Transport` trait 收口 |
| Critic B：D-008 失去 bidi-streaming 支撑 | ✅ | 重写：JSON-RPC server-initiated request；schema 含 `streamingBehavior: steer/followUp`（Pi 模型） |
| Critic B：D-010 Phase 1 不做 bridge = 无外部用户 | ✅ | Phase 1 必含 `zhive-bridge-stdio`（90 行 io::copy）+ ACP minimal conformance harness |

### Defender 推荐方案：**方案 C'**（C 的变体）

> JSON-RPC 2.0 + JSON Schema (schemars) + 抄 codex v2 结构 + ACP 作为 schema target（不是 transport 绑定）

理由：
- A 方案把 schema 绑死 ACP（ACP 自身 pre-1.0 月度 breaking）—— 太脆
- B 双协议并行最坏路径（critic B 自己都承认）
- C' 最贴合 CLAUDE.md "禁新依赖 + 不抵押未来" 硬规则
- D-010 吸收 bridge-stdio 后即获得 ACP conformance harness，外部验证回路有了

## 八、R3+R4 综合后的 D-001~D-015 终版

> 本表是 R3+R4 综合后的 D-001~D-015 最终决策版本（已被 [99-decisions](../99-decisions/) 进一步细化为权威决策文档）。

| 编号 | R3+R4 终版 |
|---|---|
| **D-001** | Phase 1 起 **7 个 crate**：`proto / core / client-native / tui / cli / xtask / bridge-stdio`。后 6 个为 R1+R2 版本（6 个）+ Phase 1 必交付的 bridge-stdio（90 行 io::copy）。砍掉 `service / server / sdk / exec / bridge-mcp / bridge-acp` 共 6 个空壳 |
| **D-002** | 保留：TUI 不依赖 core，client-native 才是 TUI 唯一上游 |
| **D-003** | **推翻 ConnectRPC**。Phase 1 RPC = **JSON-RPC 2.0 over stdio + UDS**，schema 用 `serde` 类型 + `schemars` 出 JSON Schema。`connectrpc / buffa / hyper / hyper-util / axum / tower / tower-http / prost / prost-build / prost-types / http / http-body` **全部从 Cargo.toml 移除**。ConnectRPC 退为 Phase 3 远程 transport 候选，由 `RpcTransport` trait 收口 |
| **D-004** | Phase 1 同时含 **stdio + UDS** 两种 transport，由 `Transport` trait 抽象；Windows 用 lockfile + 127.0.0.1 作为第三 transport（不是 UDS 的替身）。`default-transport` 由 CLI flag 决定，文档不承诺 |
| **D-005** | rmcp `=1.7.0` + agent-client-protocol `=0.12.1` **精确锁版本**，**仅在 `bridge-stdio` crate 内引用**（Phase 1）。core 不直接依赖。`AcpAdapter` / `McpAdapter` trait 自封一层，每次版本升级只动 adapter |
| **D-006** | 三层原语 Thread/Turn/Item 保留；schema = **serde + schemars 单一来源**，不引 prost。`Item` enum 加 `Diff` / `Terminal` / `Thought` 三类承载位（对齐 ACP `ToolCallContent`）。`Thread ↔ ACP Session` 写明"桥接表 + ID 命名空间"，不再宣称 1:1 |
| **D-007** | initialize 握手 + v1/v2 命名空间保留；capabilities 协商 `hooks / subagents / skills / permission` 独立 flag |
| **D-008** | 反向 RPC 机制保留，transport 改 **JSON-RPC 2.0 server-initiated request**（不是 ConnectRPC bidi）。`PermissionDecision` 四态 reducer `deny > defer > ask > allow`。`StreamingBehavior: steer / followUp` 二元 mode 进 schema（Pi 模型）。**新增 Subagent 权限继承规则**：父 PermissionScope 必传 + 子可缩窄不可放大 + reducer 在两侧各执行一次 |
| **D-009** | 保留 mold/lld/sccache/line-tables-only；砍 hakari；新增：`split-debuginfo = "unpacked"` 已在；移除 ConnectRPC 全家桶后 cold build 估算降至 ~25-40s |
| **D-010** | Phase 1 必含：rusqlite + JSONL rollout / hooks JSON schema / subagent context model / permission reducer / 取消传播 / **bridge-stdio 实交付** / **ACP minimal conformance harness**。Phase 2 做 bridge-mcp + bridge-acp（rmcp/ACP runtime 落地）。Phase 3 做 Web UI + 远程 + A2A 占位 |
| **D-011** | Session 持久化 = `rusqlite =0.40 + bundled` + JSONL rollout（codex 起点模式，**不是** codex 当前的多 SQLite 模式）。`Storage` trait 抽象支持后续按 domain 拆库，但 Phase 1 单库起步 |
| **D-012** | Hooks JSON schema，事件 enum `#[non_exhaustive]`，至少 14 个：`PreToolUse / PostToolUse / PostToolUseFailure / UserPromptSubmit / SessionStart / SessionEnd / SubagentStart / SubagentStop / PreCompact / PermissionRequest / Stop / Notification / Setup / ToolApprovalChange`。Hook 输入 base 字段 `session_id / cwd / hook_event_name`；subagent 上下文用 `agent_id / agent_type / parent_tool_use_id` |
| **D-013** | **不合并** Skills 与 SlashCommands。改为 **Extension manifest** 统一发现（`.zhive/extensions/<name>/manifest.toml`），manifest 内含 `kind: skill \| slash_command \| hook` 三种 namespace。filesystem-discovered + model-invoked。`settingSources` 三层（user / project / local） |
| **D-014** | `tracing` 进 Phase 1 核心：spans 覆盖 `Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint`。OTel exporter（`tracing-opentelemetry`）才 feature gate |
| **D-015** | A2A 在官方 Rust SDK 出现前**不进任何 phase 核心路径**（与拒 sled 同一把尺）。Phase 3 仅作 `AgentCard` schema 占位（HTTP+JSON 手写编码），不引 `a2a-rs` |

## 九、Phase 1 阻塞项（R3+R4 后版）

1. **删除 6 个空壳 crate 目录**：`crates/{zhive-service, zhive-server, zhive-sdk, zhive-exec, zhive-bridge-mcp, zhive-bridge-acp}`（保留 `zhive-bridge-stdio`）
2. **Cargo.toml workspace**：
   - 删 `zhive-service / zhive-server / zhive-sdk / zhive-exec / zhive-bridge-mcp / zhive-bridge-acp` 6 个 members
   - 保留 `zhive-bridge-stdio` member
   - **删 `connectrpc / connectrpc-build / hyper / hyper-util / axum / tower / tower-http / prost / prost-build / prost-types / http / http-body` 共 12 项 ConnectRPC 全家桶**
   - 新增 `schemars` + `jsonrpsee` 或自写 JSON-RPC 帧
3. 重写 [99-decisions/README.md](../99-decisions/README.md) 反映 D-001~D-015 R3+R4 终版
4. 后续配套调研（不再单独建目录，融入决策文件）：
   - ACP 0.12 minimum compliance schema 子集
   - Hook schema 完整 14+ event JSON example
   - JSON-RPC 2.0 framing + UDS server 接入示例
   - Storage trait + 单库起步的迁移路径

## 十、本次调研工作量证据轨迹

| 轮次 | Agent 数 | 关键产出 |
|---|---|---|
| 91 原 R1 | 5 | codex/Claude Code/opencode/Gemini CLI/Warp+Aider+Cline 横向调研 |
| 91 原 R2 | 4 | ACP+A2A spec / rmcp+rusqlite 选型 / Hooks 协议地基 / ConnectRPC 攻击 |
| 91 原 R3 | 1 | 整体反向批评 |
| 91 原 R4 | 2 | 协议指控反审视 + crate 拆分反审视 |
| **本轮 R1** | 4 | codex 一周 commits + Claude Code SDK 文档 + opencode acp-next 实测 + **Pi CLI（91 漏掉）** |
| **本轮 R2** | 2 | ConnectRPC 完整性实测 + rmcp+ACP 映射成本量化 |
| **本轮 R3** | 2 | 全集 critic + 协议选型 critic（10+4 条指控） |
| **本轮 R4** | 1 | Defender 逐条审视，给出 10 条硬约束 + 5 条红线 |
| **合计** | 21 | 三方独立证据三角化，关键决策（D-003/D-004/D-006/D-008/D-010/D-012/D-013）被同时验证 |

## 一手证据汇总

### codex-rs
- 工作区：<https://github.com/openai/codex/blob/main/codex-rs/Cargo.toml>
- TUI 脱 core PR：<https://github.com/openai/codex/pull/22695>
- JSON-RPC lite 声明：<https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/jsonrpc_lite.rs>
- remote_control 子系统：<https://github.com/openai/codex/blob/main/codex-rs/app-server-transport/src/transport/remote_control/protocol.rs>

### Claude Code
- Overview：<https://code.claude.com/docs/en/overview>
- Hooks：<https://code.claude.com/docs/en/agent-sdk/hooks>
- Subagents：<https://code.claude.com/docs/en/agent-sdk/subagents>
- Skills：<https://code.claude.com/docs/en/agent-sdk/skills>
- Headless / stream-json：<https://code.claude.com/docs/en/headless>
- VS Code IDE MCP server：<https://code.claude.com/docs/en/vs-code>

### OpenCode
- 仓库：<https://github.com/sst/opencode>
- OpenAPI：`packages/sdk/openapi.json`
- ACP next 实现：<https://github.com/sst/opencode/tree/dev/packages/opencode/src/acp-next>

### Gemini CLI
- 仓库：<https://github.com/google-gemini/gemini-cli>
- 扩展文档：<https://github.com/google-gemini/gemini-cli/blob/main/docs/extensions/index.md>
- A2A JS SDK：<https://github.com/a2aproject/a2a-js>

### Warp / Aider / Cline
- Warp Cargo.toml：<https://github.com/warpdotdev/Warp/blob/main/Cargo.toml>
- Warp IPC：<https://github.com/warpdotdev/Warp/blob/main/crates/ipc/Cargo.toml>
- Warp persistence：<https://github.com/warpdotdev/Warp/tree/main/crates/persistence/migrations>
- Aider：<https://github.com/Aider-AI/aider>
- Cline hooks：<https://github.com/cline/cline/tree/main/apps/vscode/src/core/hooks>

### 协议规范
- ACP spec：<https://agentclientprotocol.com/protocol/schema>
- ACP Rust SDK：<https://github.com/agentclientprotocol/rust-sdk>
- ACP 已实现 agent 清单：<https://agentclientprotocol.com/overview/agents>
- A2A spec：<https://a2a-protocol.org/latest/specification>

### Rust 库
- rmcp：<https://github.com/modelcontextprotocol/rust-sdk>
- rusqlite：<https://github.com/rusqlite/rusqlite>
- connect-rust：<https://github.com/anthropics/connect-rust>
- jsonrpsee（无 stdio）：<https://github.com/paritytech/jsonrpsee/issues/5>
- tower-lsp（stdio JSON-RPC 标杆）：<https://github.com/ebkalderon/tower-lsp>

### 工程指南
- matklad "Large Rust Workspaces"：<https://matklad.github.io/2021/08/22/large-rust-workspaces.html>
- matklad "Fast Rust Builds"：<https://matklad.github.io/2021/09/04/fast-rust-builds.html>
- Connect 协议规范：<https://connectrpc.com/docs/protocol/>
- JSON-RPC 2.0：<https://www.jsonrpc.org/specification>
