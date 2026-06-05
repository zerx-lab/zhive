---
topic: zhive 架构决策汇总
date: 2026-05-27
status: active
supersedes: 99-decisions/README.md (D-001~D-010 第一版)
---

# zhive 架构决策汇总（R3+R4 终版）

本文件是 D-001~D-015 的当前权威版本，取代上一版 D-001~D-010。背后依据是 [91-architecture-review-2026-05-27](../91-architecture-review-2026-05-27/) 的 7 轮 21 个独立 subagent 调研（R1 横向调研 + R2 实测验证 + R3 critic 反向批评 + R4 defender 反审视）。

## 决策一览

| 编号 | 主题 | 状态 |
|---|---|---|
| D-001 | Phase 1 起 7 个 crate | 实质改自上一版 12 crate |
| D-002 | TUI 不依赖 core | 保留 |
| D-003 | Phase 1 RPC = JSON-RPC 2.0，ConnectRPC 推迟到 Phase 3 候选 | **推翻**上一版 ConnectRPC 选型 |
| D-004 | Phase 1 同时含 stdio + UDS 两 transport | 实质改 |
| D-005 | rmcp/ACP 精确锁版本，仅在 bridge crate 引用 | 实质改 |
| D-006 | 三层原语 Thread/Turn/Item + serde+schemars 单 schema 源 | 修订（去 prost） |
| D-007 | initialize + v1/v2 + capabilities 协商 | 保留 |
| D-008 | 反向 RPC 走 JSON-RPC server-initiated request + steer/followUp + subagent permission inheritance | 实质改（去 ConnectRPC bidi） |
| D-009 | 编译速度组合拳（无 hakari） | 保留 |
| D-010 | 三阶段路径 + Phase 1 必含 bridge-stdio + ACP minimal harness | 实质改 |
| D-011 | rusqlite **多库**（state / logs / memories / goals）+ JSONL+Leaf rollout + Storage trait 聚合 4 子接口 | **2026-05-28 修订**（用户决策推翻单库起步） |
| D-012 | Hooks JSON schema 至少 14 事件 + `#[non_exhaustive]` | 新增 |
| D-013 | Extension manifest 统一发现，Skills/SlashCommand 是并列 namespace | 新增 |
| D-014 | tracing 进核心，OTel exporter feature gated | 新增 |
| D-015 | A2A 不进任何 phase 核心路径，仅 Phase 3 AgentCard schema 占位 | 新增（实质拒收 a2a-rs） |

---

## D-001 工作区起步规模

**决策**：Phase 1 起 **7 个 crate**：

```
crates/
├── zhive-proto           # JSON-RPC schema (serde + schemars) + 自写 framing 工具
├── zhive-core            # 引擎 + state + persistence + hooks host + server module
├── zhive-client-native   # Rust client
├── zhive-tui             # ratatui UI（仅依赖 client-native + proto）
├── zhive-cli             # 分发器
├── zhive-bridge-stdio    # 90 行 io::copy 桥接 + ACP minimal conformance integration test
└── xtask                 # 构建/迁移工具（不引入 acp-rust / rmcp）
```

**`zhive-server` 不是独立 crate**：JSON-RPC server 实现是 `zhive-core` 内一个 module（`core::server`），暴露 `Server::serve_stdio()` / `Server::serve_uds()`。这样 `bridge-stdio` 在 io::copy 时无需依赖 core，只通过 `zhive-proto` 共享 framing（解 R5 finding #1）。

**理由**：
- codex 真实演化 8→11 用 9 天、8→96 用 13 月 —— 起步过细只是 import 折磨
- 上一版 12 crate 含 6 个 Phase 1 用不到的空壳；本版砍 6 个保留 bridge-stdio
- bridge-stdio 是 Phase 1 必交付：换得 Zed / Claude Desktop / Cursor 可连入做 dogfood

**砍掉**：`zhive-service / zhive-server / zhive-sdk / zhive-exec / zhive-bridge-mcp / zhive-bridge-acp`

**依据**：[91 § 八](../91-architecture-review-2026-05-27/README.md)；codex matklad "Large Rust Workspaces"

---

## D-002 TUI 不依赖 core

**决策**：`zhive-tui` 仅依赖 `zhive-client-native + zhive-proto`，**不依赖 `zhive-core`**。

**理由**：
- codex（PR #22695, 2026-05-14）+ opencode 双重验证
- TUI 是协议的一个客户端，跟 IDE / Web / 远程同级
- 未来加 Web UI / 远程客户端零返工

**依据**：[91 § 一](../91-architecture-review-2026-05-27/README.md)

---

## D-003 协议层 = JSON-RPC 2.0（推翻 ConnectRPC）

**决策**：Phase 1 core ↔ 所有客户端 RPC = **JSON-RPC 2.0 over stdio + UDS**。Schema 用 `serde` Rust 类型为 source-of-truth，`schemars` 生成对外 JSON Schema。ConnectRPC 退为 **Phase 3 远程 transport 候选**之一，由 `RpcTransport` trait 收口。

**Cargo.toml 直接删除**：`connectrpc / connectrpc-build / hyper / hyper-util / axum / tower / tower-http / prost / prost-build / prost-types / http / http-body` 共 12 项。

**JSON-RPC framing：自写**（R5 finding #3 确认）。理由：`jsonrpsee` 至今不支持 stdio transport（[issue #5](https://github.com/paritytech/jsonrpsee/issues/5)），而 D-004 Phase 1 必须 stdio。自写 framing 实现量 < 200 行（length-delimited Content-Length header 风格，照搬 LSP 帧格式 + serde_json codec）。统一放在 `zhive-proto::framing` module。

**新增依赖**（已在本决策书取得确认，符合 CLAUDE.md 红线 1）：
- `schemars` — 生成对外 JSON Schema（仅 zhive-proto 用）

**JSON-RPC spec 依据**（D-008 关联）：JSON-RPC 2.0 没有限制 "client" 与 "server" 角色，wire 层只有 Request/Response/Notification（[spec § 4 / § 5](https://www.jsonrpc.org/specification)），任一端皆可发 Request。LSP `$/`reverse request 和 ACP `permission/request` 都按此实现。

**推翻理由**（R2+R3 一手证据）：
- anthropics/connect-rust 73 天 11 release，0.4→0.5→0.6 三连 breaking minor
- 强制 anthropics-only `buffa` codegen，无法用 prost / tonic-build / pbjson 生态
- Reflection 未实现（issue #129 无 maintainer 回应）
- gRPC / gRPC-Web conformance 在 CI 不跑
- 业内零同行用作 core ↔ TUI 协议（codex / Claude Code / opencode / Pi / Gemini CLI / Warp 全在 JSON-RPC 或自有协议阵营）
- 91 修订本身已让 Phase 1 fallback JSON-RPC —— ConnectRPC "保留" 实质只为 Phase 3 抵押 Phase 1 复杂度

**未来需要 binary RPC 时**：抽 `RpcTransport` trait，让 ConnectRPC / tonic / jsonrpsee 任选其一。

**依据**：[91 § 七 Critic A-5 / B-D003](../91-architecture-review-2026-05-27/README.md)

---

## D-004 本地 transport 同时含 stdio + UDS

**决策**：Phase 1 同时实现 **stdio 与 UDS 两种 transport**，由 `Transport` trait 抽象。default 由 CLI flag 决定。Windows 用 lockfile + 127.0.0.1 作为第三 transport（平行存在，不是 UDS 的替身）。

**理由**：
- codex 同时上 stdio + unix_socket + websocket + remote_control 是已验证的 daemon agent 刚需
- IDE / WSL / 容器 / 远程隧道四种场景互斥，单 transport 必撞墙
- 单选 UDS + "Windows lockfile 补丁"是过早收敛的报应
- stdio 实现仅 ~90 行（io::copy），不构成工程负担

**stdio 用法**：
- `zhive bridge-stdio` 给 MCP / ACP 客户端 spawn
- 远程 SSH 隧道
- 容器内通信

**UDS 默认路径**：`$XDG_RUNTIME_DIR/zhive.sock` 或 `/tmp/zhive-<uid>.sock`，文件权限 0600

**依据**：[91 § 七 Critic B-D004](../91-architecture-review-2026-05-27/README.md)

---

## D-005 MCP/ACP 仅在 bridge crate 内引用 + 精确锁版本

**决策**：
```
crates/zhive-bridge-stdio (Phase 1)
  ├─ deps: agent-client-protocol = "=0.12.1"  (精确锁，不是 ^)
  └─ deps: rmcp = "=1.7.0"                    (精确锁，不是 ^)

crates/zhive-bridge-mcp (Phase 2)
crates/zhive-bridge-acp (Phase 2)
```

`AcpAdapter` / `McpAdapter` trait 自封一层，每次版本升级只动 adapter。

**理由**：
- ACP 0.12.0 已发生 breaking（McpAcpTransport 移除）+ 月度 minor，必须精确锁
- rmcp 1.x 用 `#[non_exhaustive]` 防破坏，可放心 1.7.0 起步
- bridge 是 codex 已验证的 hexagonal adapter 模式（Pi-acp 也是同模式）
- 上层 zhive-core 不直接依赖 rmcp / acp-rust → 上游 breaking 时不污染 core

**映射工程量已实测**（R2）：
- rmcp → zhive Item：~150-250 行
- ACP SessionUpdate → zhive Item：~300-400 行
- ContentBlock ↔ RawContent：~50 行（1:1 同构）

**依据**：[91 § 二 / § 七 Critic A-4 / A-6](../91-architecture-review-2026-05-27/README.md)

---

## D-006 三层原语 + serde+schemars 单一 schema 源

**决策**：
```
Thread → 持久会话
  └─ Turn → 一次用户输入 + 全部 agent 响应
       └─ Item → reasoning / tool_call / exec / file_edit / agent_message
                / diff / terminal / thought (R3+R4 新增三类)
```

Schema = **`serde` Rust 类型 + `schemars` 生成 JSON Schema**，单一来源。**不引入 prost / protobuf 工具链**。

`Thread ↔ ACP Session` 用桥接表 + ID 命名空间（**不是 1:1**）。

`Item::Diff / Terminal / Thought` 三类是为对齐 ACP `ToolCallContent` 新增的承载位。

**理由**：
- codex v2 也是 TS schema 单源（不用 prost）
- CLAUDE.md 禁新依赖，prost+prost-build+prost-types 是三个新 crate
- schemars 已在 Rust 生态成熟（jsonrpsee / utoipa 等都用）
- ACP `SessionUpdate` 10 个 case + MCP `RawContent` 5 个 case 都能映射到 zhive Item（R2 已实测）
- MCP 无 Turn 概念 → bridge 侧合成 Turn 边界（在 `tools/call` 入口起、`CallToolResult` 收）

**依据**：[91 § 七 Critic A-7](../91-architecture-review-2026-05-27/README.md)；R2 rmcp+ACP 映射工程量评估

---

## D-007 强制 initialize 握手 + 协议版本化 + capabilities 协商

**决策**：
- 第一个 RPC 必须是 `initialize`
- 协议分 `v1` / `v2` 命名空间
- `capabilities` 协商每项独立 flag：`hooks / subagents / skills / permissions / extensions / streaming_behavior`

**理由**：
- codex 本周一次性删 v1 plumbing（6+ PR）—— 反向证明双命名空间 + 版本化是正解
- Claude Code / ACP 都用同样握手
- 独立 capability flag 让客户端能力分级演进

**依据**：[91 § 一](../91-architecture-review-2026-05-27/README.md)

---

## D-008 反向 RPC（JSON-RPC server-initiated request）

**决策**：
- Transport = **JSON-RPC 2.0 server-initiated request**（不是 ConnectRPC bidi）
- 审批与事件走同一 stream，不同 message type
- `PermissionDecision` 四态 reducer：**deny > defer > ask > allow**
- Schema 含 `StreamingBehavior: steer | followUp` 二元 mode（Pi 模型）
- **Subagent 权限继承规则**（R4 新增硬约束）：
  - 父 `PermissionScope` 必传给 child
  - 子可缩窄 scope 不可放大
  - reducer 在父子两侧各执行一次
- Schema 字段命名与 Claude Code Agent SDK 对齐：`{ systemMessage?, continue?, hookSpecificOutput: { permissionDecision, permissionDecisionReason, updatedInput, additionalContext, updatedToolOutput } }`

**理由**：
- D-003 去 ConnectRPC 后，bidi-streaming 支撑塌；JSON-RPC server-initiated request 是 LSP / ACP 验证过的形态
- Pi `streamingBehavior` 区分"中途介入"与"补充输入"，比 codex 单一 reverse-request 表达力强
- Claude Code 文档明确"父用 bypassPermissions / acceptEdits 时所有 subagents 强制继承"是已知安全雷区，schema 必须反映

**依据**：[91 § 七 Critic A-9 / B-D008](../91-architecture-review-2026-05-27/README.md)；Pi RPC types

---

## D-009 编译速度组合拳

**决策**：从第一天就上完整方案，不要等"以后再优化"。

清单：
- workspace + 共享 dependencies
- `[profile.dev] debug = "line-tables-only"` + `opt-level = 1` for deps
- `mold` (Linux) / `lld` (macOS) linker
- `sccache` rustc-wrapper
- 所有重型可选能力走 feature gate
- **砍 `cargo-hakari`**（crate 数收敛到 7，未到痛点）

D-003 去 ConnectRPC 全家桶后，cold build 估算从 90-140s 降至 ~25-40s。

**实测（2026-05-27, `cargo clean && cargo build --workspace`，开发机，无 mold / sccache 加速）：** `16.57s`（比估算还快 ~50%；比 ConnectRPC 时代下界 90s 减 ~82%）。所有 6 个内部 crate + xtask 编译完成，外加 llmsdk git 依赖。

**依据**：[91 § 八 D-009](../91-architecture-review-2026-05-27/README.md)；matklad "Fast Rust Builds"

---

## D-010 三阶段渐进路径

**Phase 1（最小可用，Phase 1 必含项）**
- `proto/`：JSON-RPC 2.0 schema（serde + schemars）含 `initialize / TurnStart / TurnEvent / ToolApproval / ReverseRequest`
- `zhive-core`：引擎骨架 + state + persistence + hooks host + permission reducer + 取消传播
- `zhive-server`：embed 在 core 内，JSON-RPC server over UDS + stdio
- `zhive-client-native`：Rust client
- `zhive-tui`：ratatui 最小界面
- `zhive-cli`：分发器
- **`zhive-bridge-stdio`**：90 行 io::copy，让外部 ACP / MCP 客户端 spawn
- **ACP minimal conformance harness**：放在 `zhive-bridge-stdio` 的 `tests/acp_conformance.rs` 集成测试中（**不是 xtask**，避免违反 D-005 字面约束。解 R5 finding #2）。最小验收集：
  - `initialize` 双向握手
  - `session/new` + `session/prompt` 一轮 turn
  - `session/update` 至少 3 种类型：`UserMessageChunk / AgentMessageChunk / ToolCall`
  - `session/cancel` 中断

**Phase 2（生态接入）**
- `zhive-bridge-mcp`：rmcp 1.7 runtime 落地
- `zhive-bridge-acp`：agent-client-protocol 0.12.1 runtime 落地（read+write）
- `zhive-exec`：headless 模式
- Persistence 按 domain 拆库（Storage trait 已留口）

**Phase 3（扩展）**
- Web UI（gRPC-Web 复用 schema 或 SSE）
- 远程 TLS / 云沙箱
- ConnectRPC 候选评估（如果生态稳定到 1.0）
- A2A AgentCard schema 占位（HTTP+JSON 手写，不引 a2a-rs）

**理由**：
- "Phase 1 不做 bridge = Phase 1 无外部用户"，没法 dogfood，schema 决策缺压力测试 → Phase 2 翻译层吸收所有 schema mismatch（91 自己反对的 N×M 翻译矩阵）
- bridge-stdio 是 ~90 行 io::copy，Phase 1 加这 90 行换"ACP 客户端可连入"是极高 ROI

**依据**：[91 § 七 Critic B-D010](../91-architecture-review-2026-05-27/README.md)

---

## D-011 Session 持久化 = rusqlite 多库 + JSONL rollout

> **2026-05-28 修订**：上一版"Phase 1 单库起步，Storage trait 留口"已废。直接采用 codex 当前演进的多库结构，理由见 § 修订理由。

**决策**：
- `rusqlite =0.40 + bundled`（精确锁版本，不切 sqlx —— codex 用 sqlx 是其工程偏好，与 D-011 拒绝 sqlx-sqlite 的"破坏 cargo check -p 工作流"理由不冲突，结构借鉴 ≠ 实现照抄）
- JSONL rollout 作 source-of-truth（独立 `zhive-rollout` 子 module 或同 core 内 module，参考 codex `rollout/` crate）
- **SQLite 从 Phase 1 起就 4 库分离**，结构对齐 codex `codex-rs/state/`：

  | DB 文件 | 用途 | migrations 目录（zhive） | 对照 codex |
  |---|---|---|---|
  | `state.db`    | threads / sessions / agent_jobs / 主索引 | `crates/zhive-core/migrations/state/` | `codex-rs/state/migrations/` |
  | `logs.db`     | 结构化日志（tool exec / error / event 流） | `crates/zhive-core/migrations/logs/` | `codex-rs/state/logs_migrations/` |
  | `memories.db` | 跨 session 长期记忆（per Pi+Claude Code 模式） | `crates/zhive-core/migrations/memories/` | `codex-rs/state/memory_migrations/` |
  | `goals.db`    | thread-level goals / TODO | `crates/zhive-core/migrations/goals/` | `codex-rs/state/goals_migrations/` |

- `Storage` trait 不是"留口"，而是**Phase 1 必交付的 4 库聚合接口**：
  ```rust
  trait Storage {
      fn state(&self) -> &StateDb;
      fn logs(&self) -> &LogsDb;
      fn memories(&self) -> &MemoriesDb;
      fn goals(&self) -> &GoalsDb;
  }
  ```
- 每个 DB 独立 connection pool（rusqlite + `r2d2-sqlite` 或 `deadpool-sqlite`，本任务调研定）
- **Leaf 指针**采纳 Pi 模型：JSONL 不只 append，最后一条可写 `leaf` entry 指向当前分支头，支持 fork（[B3 deliverable 落地](../../plans/phase1-core-native-research/phase1-core-native-research.md#b3--persistencerusqlite--jsonl-rollout)）

**修订理由**（2026-05-28，用户决策）：

- "Phase 1 单库 → Phase 2 拆多库"是上一版基于"codex 多库是演进中段"的论证。但**早晚要拆 = 一开始就拆代价更小**：
  - schema 跨库 migration 是高成本工程（外键失效、事务边界变化、备份策略变化）
  - 4 库并行从 0 写 vs 1 库写完再拆，前者多写 ~200 行 ddl，后者要做 data migration 工具 + 测试 + 灰度
  - codex 自己的 PR #24591 ~3000 行 diff 就是这个学费
- 上一版引用的"红线 8（不得因 codex 拆多 SQLite 就在 Phase 1 拆多库）"同步废除（见 § 红线）

**拒绝的候选**（保留）：sled / diesel / sqlx-sqlite / sea-orm / redb / 纯 JSONL —— 理由同上一版

**与 91 § 二 Critic A-1 的关系**：A-1 反对"拆多 DB"，本次修订是用户基于工程总成本的反向决策；A-1 的论据未被否定（"codex 演进中段"是事实），但权衡换了一把尺。

**依据**：[91 § 二 Session 持久化](../91-architecture-review-2026-05-27/README.md)（论据轨迹保留）；2026-05-28 用户决策；codex `state/` 当前结构（35+1+2+1 migrations，4 库分离）

---

## D-012 Hooks JSON schema

**决策**：
- Hooks 协议 = **JSON schema**，事件 enum `#[non_exhaustive]`
- Phase 1 至少 **14 个事件**：
  ```
  PreToolUse / PostToolUse / PostToolUseFailure / UserPromptSubmit /
  SessionStart / SessionEnd / SubagentStart / SubagentStop /
  PreCompact / PermissionRequest / Stop / Notification /
  Setup / ToolApprovalChange
  ```
- Reserved for Phase 2/3（5 个）：`PostToolBatch / TeammateIdle / TaskCompleted / ConfigChange / WorktreeCreate / WorktreeRemove`
- Hook 输入 base 字段：`session_id / cwd / hook_event_name`
- Subagent 上下文：`agent_id / agent_type / parent_tool_use_id`

**理由**：
- Claude Code 19 个事件实测命中 R1（hooks 文档）
- Pi 用类似事件集（session_start / tool_call / before_compact）
- 缺 PostToolUseFailure（错误处理）/ PreCompact（窗口压缩）/ Notification（异步通道）= toy hooks
- `#[non_exhaustive]` 是零成本前向兼容

**依据**：[91 § 三 + § 七 Critic A-3](../91-architecture-review-2026-05-27/README.md)

---

## D-013 Extension manifest 统一发现，Skills/SlashCommand 并列 namespace

**决策**：
```
.zhive/extensions/<name>/manifest.toml
  kind: skill | slash_command | hook
  ...
```

- filesystem-discovered + model-invoked
- `settingSources` 三层（user `~/.zhive/` / project `.zhive/` / local `.zhive.local/`）
- Skills 与 SlashCommands **不合并**，是 manifest 下两个并列 namespace（Pi `source: extension|prompt|skill` 模型）

**理由**：
- "Skills 替代 SlashCommands" 在 R1 一手 docs 中被反驳（slash commands 仍存在为 legacy）
- Pi Extension manifest 是公开 schema 证据，三合一聚合范式 ROI 高
- 强行合并会导致 Skills 发现器和 SlashCommand dispatcher 二选一返工

**依据**：[91 § 七 Critic A-2](../91-architecture-review-2026-05-27/README.md)；R1 Pi CLI 调研

---

## D-014 tracing 进核心，OTel exporter feature gate

**决策**：
- `tracing` 进 Phase 1 核心，spans 强制覆盖：`Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint`
- `tracing-opentelemetry` exporter（OTLP gRPC/HTTP）为 **feature gate**
- `tracing-subscriber` 仅启 `fmt + env-filter`，OTel 才进可选

**理由**：
- Gemini CLI "OTel 一等公民" 是 Gemini 的选择，不必照搬其 runtime
- 但 spans 是 Rust 生态事实标准（CLAUDE.md 也要求 `?` + `thiserror`，错误链需要 tracing 才能复盘）
- D-008 的反向 RPC + permission reducer + subagent 边界这些 D-010 强调的核心机制如果第一天没有 trace，复盘几乎不可能

**依据**：[91 § 七 Critic A-10](../91-architecture-review-2026-05-27/README.md)

---

## D-015 A2A 不进任何 phase 核心路径

**决策**：A2A 在官方 Rust SDK 出现前**不进任何 phase 核心路径**。Phase 3 仅作 `AgentCard` schema 占位（HTTP+JSON 手写编码），不引 `a2a-rs` 0.2.x crate。

**理由**（与拒 sled 同一把尺）：
- a2a-rs 0.2.0：85 stars、单维护者、pre-1.0、月更新可能停滞
- 91 自己拒绝 sled 用的标准就是"单维护者 + 停滞"
- A2A 官方 SDK 列：Python / Go / JS / Java / .NET（**无官方 Rust**）
- AgentCard 是 JSON 即可，~50 行手写编码够用

**依据**：[91 § 二 A2A + § 七 Critic A-8](../91-architecture-review-2026-05-27/README.md)

---

## 后续调研项（融入决策落地，不再单独建目录）

- ACP 0.12 minimum compliance schema 子集（zhive 必须满足的最小 ACP 兼容性）
- Hook schema 完整 14+ event JSON example
- JSON-RPC 2.0 framing + UDS server 接入示例
- Storage trait + 单库起步的迁移路径
- Sandbox 层抽象（Landlock / Seatbelt / Job Object / 远程容器）
- LLM provider 抽象（统一 OpenAI / Anthropic / 本地模型）
- 配置层（layered TOML 解析 + 校验）
- 鉴权（本地 socket 文件权限 vs 远程 token）
- 团队协作 / 远程 session 共享

---

## R5 一致性审查后的待补漏洞（Phase 2 之前必须解决）

> 已修补的 3 处硬伤：D-001 server 归属 / ACP harness 归属 / framing 实现路径。下面是 R5 找出的剩余 7 处可放到 Phase 2 之前修的漏洞。

1. ~~**D-008 `StreamingBehavior` 取消状态机**：仅声明 `Steer | FollowUp` 二元 mode 不够。落地前需补：`Steer` 时 in-flight tool_call 是否撤销 / 已发 reverse-request 是否回收 / Turn 边界如何重置。建议参考 Pi `pending extension UI requests` Map 设计。~~ —— **2026-06-05 主体已解决**：`Steer` 语义已在 `crates/zhive-core/src/engine/turn.rs:413-420` 定义并测试——`Steer` **不**撤销 in-flight tool_call（仅为下一次 LLM 调用 seed 注入），abort 走独立路径（`InjectionQueues::abort` / `CancellationTree`），故 `Steer` 域内无 reverse-request 需回收、Turn 边界由现有 turn 循环重置。**剩余开放项**仅"通用 `session/cancel` 路径下已发 reverse-request 的回收"，与 `Steer` 模式无关，单独跟踪。
2. **D-013 Extension manifest 完整字段** + **与 D-012 Hooks 来源优先级**：当前 manifest 只写 `kind: skill | slash_command | hook`，未定义其余字段（`description / model_invocable / allowed_tools / disable_in_subagent / ...`）。也未规定"hook 通过 manifest 注册 vs settings 顶层注册"的优先级 / 覆盖规则。
3. **验收标准缺失**：D-007 / D-008 / D-011 / D-014 只说"做"未说"如何证明做到"。每条决策应附 acceptance test 文件路径（如 `crates/zhive-core/tests/permission_reducer.rs`）。
4. **Phase 1 占位决策缺失**：91 § 二待定项有 5 大块——LLM provider 抽象 / Sandbox 层 / 鉴权 / 配置层 / 远程 session 共享。当前 D-001~D-015 均未提供 Phase 1 占位决策。最少要：core 怎么对接 `llmsdk`、本地权限模型（UDS 文件权限够不够、token 何时需要）。
5. **D-005 上游 patch 跟版流程**：精确锁 `=0.12.1 / =1.7.0 / =0.40` 后，上游出安全 patch 时谁负责追版？建议加"每月一次 `xtask check-upstream` 跑过一遍 diff"。
6. **91 § 二旧 ConnectRPC 结论需打 superseded 标**：91/README.md § 二 "结论：D-003 不全盘推翻，但加 feature gate" 与 § 八 终版 "推翻 ConnectRPC" 字面相反，已在 § 二行内补 `> ⚠️ 已被 § 八 取代` 提示。
7. **Cargo.toml 阻塞项 owner / deadline**：D-001 / D-003 列了删 6 crate + 删 12 项依赖的清单，但未指定谁负责执行。建议本次提交后立刻跑一遍。

## 红线（CLAUDE.md 已禁 + 本次新增）

CLAUDE.md 原有：
1. 禁止新增 dependency（需 PR 说明 + 确认）
2. 禁止 `unsafe`（显式批准除外）
3. 禁止 `unwrap()` / `expect()` 非测试代码
4. 公开 API 必须 doc comment + doctest/example

本次 review 新增（R4 defender 给的 5 条红线 → 现存 4 条）：
5. 不得新增 prost / ConnectRPC runtime / a2a-rs 进 Phase 1 核心依赖
6. 不得让 Phase 1 出现"bridge crate 不存在但要求依赖只在 bridge crate 内"的字面矛盾
7. 不得用双标尺评估第三方 crate（sled 与 a2a-rs 同尺）
8. ~~不得因"codex 在拆多 SQLite"就在 Phase 1 拆多库~~ —— **2026-05-28 废除**：D-011 已修订为多库起步，理由见 D-011
9. 不得保留任何"为 Phase 3 抵押 Phase 1 复杂度"的 feature gate

本次新增（2026-05-28 Pi 调研后）：
10. 公开 hook event base 字段必含 `registered_by: ExtensionRef`（Pi 反例：missing source metadata 导致后续无法定位 hook 注册者）
11. tool_call hook 允许 mutate input 时，**必须重新过 schema 验证**（Pi 反例：mutate 后不验证 → 工具崩溃来源）
