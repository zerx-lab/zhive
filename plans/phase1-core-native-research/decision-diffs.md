---
plan: phase1-core-native-research
date: 2026-05-28
status: 已定稿（2026-05-28 终审）
scope: A1-A5 / B1-B10 / C1-C4 调研收尾（19 个 deliverable 汇总）
purpose: 集中回流调研期间发现的「决策修订建议 / 决策冲突警告 / 风险触发 / 待用户决策 / wire schema 新增 / Phase 1 占接口不实装项」，由用户决定是否更新 research/99-decisions/README.md
rule: |
  2026-05-28 终审：所有 ⏳ 决策项基于 deliverable 一手源码调研结论 + 推荐方向定稿。
  采纳的修订需后续手动同步 research/99-decisions/README.md（落地 PR 序列启动前完成）。
  本文件不直接修改决策原文，决策结果以 § 0 表 + § 1-§ 4 各节"决策点"字段为准。
references:
  - plan §7.4 退出条件（"能不能给一个没看过 5 个 repo 的工程师直接派活写 Engine::run_turn()"）
  - plan §10 决策回流
  - plan §11 状态由用户后续手动更新
---

# 决策修订建议汇总（A1-A5 / B1-B10 / C1-C4）

## 0. 一目了然（决策表）

| 类别 | 锚 | 修订建议 | 来源 deliverable | 终审结论 (2026-05-28) |
|---|---|---|---|---|
| 决策修订 | D-006 | Item enum case 数 8 → 14（补 UserMessage / Plan / AvailableCommands / ModeChange / ContextCompaction / SystemNotice） | A1 §2 / §8 OP-1 | ✅ 采纳 |
| 决策修订 | D-006 | discriminator 选 `itemKind`（避开 ACP `sessionUpdate` / codex `type` / `ToolCall.kind` 字段冲突） | A1 §2.1 决策冲突警告 | ✅ 采纳（代码已实装） |
| 决策修订 | D-007 | "双命名空间共存" 重定义为「源码模块层 v1/v2 + wire 层无前缀 + capability flag」三件套；method 字符串保持裸 `initialize`，不出现 `v1/` 前缀 | A2 §6.Q3 / TODO A2.1 / A2.6 | ✅ 采纳 |
| 决策修订 | D-007 | 纳入 `initialized` notification（codex 有，ACP 无） | A2 TODO A2.3 | ✅ 采纳 |
| 决策修订 | D-008 | 二元 `StreamingBehavior` → **三队列模型**（Steer / FollowUp / NextTurn），每队列独立 QueueMode，NextTurn 跨 abort 保留；新增 `Cancelled` outcome 硬约束 | A3 顶警告 + §11 词条草案 | ✅ 采纳 |
| 决策修订 | D-008 | Subagent 继承靠 `SubagentDefinition` 字段缺省，**不存在** wire `inherited_permissions` 字段 | A3 §7.3 / §9 Q5 | ✅ 采纳 |
| 决策修订 | D-012 | reserved 5 个调整：`WorktreeCreate / WorktreeRemove` 下沉 Phase 3；新增 `PostCompact` / `PreProviderRequest` / `PostProviderResponse` / `PreBranchSummary` / `PostBranchSummary` | A4 顶警告 + §8 TODO A4-D1 / B9 TODO B9-2 | ✅ 采纳 |
| 决策修订 | D-012 | `ToolApprovalChange` 保留为 zhive 自有事件（permission_mode 切换审计，A4 §3 已验证场景真实） | A4 顶警告 + TODO A4-D2 | ✅ 保留方案 (a) |
| 决策修订 | D-012 | `Setup` 是 Claude Code TS-only；Phase 1 不实装，改 reserved | A4 顶警告 + TODO A4-D3 | ✅ 改 reserved |
| 决策修订 | D-012 | 新增第 15 事件 `PhaseTransition { from, to, thread_id }`（B1 提议；与 `#[non_exhaustive]` 兼容） | B1 §6.7 / B1-3 / B5 TODO B5-8 | ✅ 采纳 |
| 决策修订 | D-013 | `kind: skill \| slash_command \| hook` → **`kind: extension \| prompt \| skill`**；hook / slash_command / tool / shortcut / flag 全部作为 `extension` 的 sub-section（R-D013） | A5 顶警告 + §10 R-D013 | ✅ 采纳 |
| 决策修订 | D-013 | "hook 必须挂 extension manifest，不允许 settings 顶层裸注册"（联动红线 10）（R-D013-2） | A5 §10 R-D013-2 | ✅ 采纳 |
| 决策修订 | D-013 | `entrypoint` 字段 Phase 1 仅承认 `"builtin"`，第三方 entrypoint 推 Phase 2（R-D013-3 隐含） | A5 §2 表 + TODO A5-1 | ✅ 采纳 |
| 决策修订 | D-014 | 6 个 span 之外新增 `zhive.compaction` 容器 span + `zhive.branch_summary` 容器 span（覆盖 EnginePhase::Compaction / BranchSummary 期间） | B9 TODO B9-1 / B9-2 | ✅ 采纳 |
| 风险触发 | R-2 | rusqlite =0.40 + bundled cold release build **实测 78-80s**（>60s 阈值，未达 >2min 极端值）；是否接受 release 慢路径 | B3 §8 / TODO B3-4 | ✅ 接受 (2026-05-28) |
| 风险触发 | R-7 | rusqlite connection pool 选型：方案 **d**（`Arc<Mutex<Connection>>` × 4 库，0 新依赖）起步；中期演进方案 c（自写 mini pool） | B3 §6 / TODO B3-2 | ✅ 方案 d (2026-05-28) |
| 风险触发 | R-8 | 跨库一致性：4 库无原子事务。采纳 D-011 "JSONL source of truth"，state.db 是衍生索引；崩溃恢复伪码已落地（B3 §7.3） | B3 §7 / B2 TODO B2-7 | ✅ 方案已选 |
| 风险未触发 | R-3 | tower-lsp **不再作为蓝本**，已切换至 `async-lsp` 设计 + `codex` 工程双蓝本；不引 async-lsp 作 dep（仅 design reference） | B4 §2 / §10 | ✅ 已落槌 |
| 风险未触发 | R-4 | llmsdk trait 4 个必需能力（completion / stream / tool_call / reasoning）全覆盖；适配纯在 fold 逻辑 | B10 §0 / §5 | ✅ 已落槌 |
| 风险触发 | R-6 | Phase 1 不装 `tracing-opentelemetry`，走方案 **b**（stub trait + 文档占位 + 字段命名预对齐 OTel semconv）；Phase 2 再加 dep | B9 §3 / TODO B9-6 | ✅ 方案 b (2026-05-28) |
| 红线 1 | jsonschema | `jsonschema = "0.18"`（红线 11 mutate 后重验证必需）——走 cargo add | B5 TODO B5-2 | ✅ 同意 cargo add (2026-05-28) |
| 红线 1 | futures | `futures = "0.3"` **已在 workspace**（`Cargo.toml:46`）；core/native-client/tui 三 crate 均已 `{ workspace = true }` 引用，**不触红线 1** | B5 TODO B5-3 | ✅ 用现有 workspace dep (2026-05-28) |
| 红线 2 | XDG_RUNTIME_DIR | 取消 `libc::getuid()` unsafe fallback；改强制要求 `XDG_RUNTIME_DIR`，无则启动报错 | B4 TODO B4-2 | ✅ 方案 b (2026-05-28) |

### Phase 1 占接口不实装的 6 项（全部 ✅ 保留接口位，2026-05-28）

| # | 项 | 决定 |
|---|---|---|
| 3.1 | Windows lockfile + 127.0.0.1 transport | ✅ 保留 CLI flag + 运行时 `TransportNotImplementedInPhase1` |
| 3.2 | client 自动重连 | ✅ 保留 `Disconnected` 终态，caller 走 `connect_*()` 重建 |
| 3.3 | ACP 0.12 reserved 7 个反向 method（`fs/*`, `terminal/*`） | ✅ 默认 `-32601 method_not_found`，bridge crate 挂 handler |
| 3.4 | tracing-opentelemetry layer | ✅ 保留 `#[cfg(feature = "otel")]` gate + stub trait |
| 3.5 | Subprocess hook executor | ✅ 保留 `HookExecutor::Subprocess(...)` enum variant |
| 3.6 | Extension 热重载 trigger | ✅ 保留 scope token + `unregister_scope`，不暴露 `/reload` CLI |

---

## 1. 决策修订建议（D-XXX）

### 1.1 D-006: Item enum case 数 8 → 14

**来源**：A1 §2 / §8 TODO OP-1

**当前 D-006 原文**（`research/99-decisions/README.md` §D-006）：
> `Item` 列 8 case：`reasoning / tool_call / exec / file_edit / agent_message / diff / terminal / thought`

**建议修订**：将 8 case 扩到 **14 case**，并明确「Item 字段集 = bridge 必需映射点的交集」原则：

```text
1. UserMessage          # 用户输入（D-006 未列；codex UserMessage + ACP UserMessageChunk 必需）
2. AgentMessage         # 助手回复（D-006「agent_message」）
3. Reasoning            # 推理（D-006「reasoning」）
4. AgentThought         # 内部思考（D-006「thought」；ACP AgentThoughtChunk）
5. ToolCall             # 工具调用（D-006「tool_call」；含完整 ACP ToolCall 字段集）
6. CommandExecution     # shell 执行（D-006「exec」）
7. FileEdit             # 文件编辑（D-006「file_edit」）
8. Diff                 # 文件 diff 独立承载位（D-006「diff」）
9. Terminal             # 终端嵌入（D-006「terminal」）
10. Plan                # 计划（D-006 未列；ACP「Plan」+ codex「Plan」均有）
11. AvailableCommands   # 可用命令快照（ACP AvailableCommandsUpdate 必需）
12. ModeChange          # 模式切换（ACP CurrentModeUpdate 必需）
13. ContextCompaction   # 上下文压缩（codex 有；A4 hook PreCompact/PostCompact 对照）
14. SystemNotice        # 系统通知（zhive 自有；D-012 Notification hook 落地为 item）
```

**影响**：
- `zhive-proto` Item enum 增加 6 case
- A1 §6 wire 草图、A4 hook 映射、B1 engine fold 逻辑均按 14 case 实装
- discriminator 顺带建议改 `kind`（见 §1.2）

---

### 1.2 D-006: discriminator 字段名 `itemKind`（不是 `kind` / `type`）

**来源**：A1 §2.1 决策冲突警告 + 2026-05-28 实装时发现的内部矛盾。

**当前 D-006 原文**：D-006 未明确 discriminator 字段名。

**实装实测发现**（zhive-proto::domain 2026-05-28）：A1 §2.1 建议 `#[serde(tag = "kind")]`，但 §3 字段表又把 `ToolCall.kind: ToolKind` 当字段；serde 不允许 enum 的 tag 与 variant 内部字段同名。编译时报 `variant field name "kind" conflicts with internal tag`。

**建议修订**：在 D-006 内显式声明 `Item` 使用 `#[serde(tag = "itemKind")]`，值用 snake_case：
- 不用 `type`（与 codex v2 `ThreadItem` `#[serde(tag = "type")]` 冲突；与 schemars `"type"` 元字段易混）
- 不用 `sessionUpdate`（ACP 专用，bridge 会同 key 撞名）
- 不用 `kind`（与 `ToolCall.kind: ToolKind` serde 字段冲突）
- `itemKind` 同时满足 A1 §2.1 的两个理由（不与 ACP/codex 撞名，不与 schemars 元字段冲突）

**决策点**：✅ **已实装为 `itemKind`** (2026-05-28，zhive-proto::domain.rs:300)。建议同步更新 D-006 原文。

**影响**：A1 §6 草图（line 468）需要回改一笔；B2 内存模型、B3 schema 全部以 `itemKind` 为准。

---

### 1.3 D-007: "双命名空间共存" 重定义

**来源**：A2 §6.Q3 / TODO A2.1 / A2.6

**当前 D-007 原文**：标题写「initialize + v1/v2 + capabilities 协商」，但 plan §4 A2 描述「v1/v2 命名空间共存」一词易被理解为 wire 前缀。

**建议修订**：把「v1/v2 命名空间共存」重定义为三件套：
1. **源码模块层**：v1 / v2 type 分别在 `protocol::v1::*` / `protocol::v2::*`
2. **wire 层**：method 字符串保持裸 `initialize` / `initialized` / `thread/start` / `permission/request` —— **从不出现 `v1/` 或 `v2/` 前缀**（与 codex / ACP / LSP 生态对齐）
3. **协商层**：`protocolVersion: u16` + `capabilities.experimental_api: bool` 共同决定运行时行为

**影响**：A2 §5 §6.Q3、B4 server router 注册表（method 不带版本前缀）。

---

### 1.4 D-007: 纳入 `initialized` notification（codex 有，ACP 无）

**来源**：A2 TODO A2.3

**建议修订**：D-007 文本增加 `initialized` notification（客户端发出，表示已完成 capability 协商）。

**理由**：与 codex 对齐，给握手收尾一个明确的「双方就绪」信号。ACP 无此 notification 不阻塞兼容（zhive 不发给 ACP bridge 即可）。

---

### 1.5 D-008: 二元 `StreamingBehavior` → 三队列模型

**来源**：A3 顶警告 / §11 词条修订草案 / R5 finding #1

**当前 D-008 原文**：
> Schema 含 `StreamingBehavior: steer | followUp` 二元 mode（Pi 模型）

**建议修订**（A3 §11 verbatim）：
```diff
- Schema 含 `StreamingBehavior: steer | followUp` 二元 mode（Pi 模型）
+ Schema 含 **三队列模型**（取代二元 mode，对齐 Pi agent-harness.ts:183-187）：
+   - `Steer`：turn 执行期间注入，对下一个 LLM 请求立即可见
+   - `FollowUp`：agent 无更多 action 时注入
+   - `NextTurn`：abort **不清空**，跨 turn 保留（恢复 / 重发关键）
+   每队列独立 `QueueMode { All | OneAtATime }`，NextTurn 无 mode（永远 All）。
+   Wire 上 `streamingBehavior?: "steer" | "followUp"` 二元仅覆盖前两个；
+   `NextTurn` 由独立 `session/next_turn` RPC method 驱动。
+ `abort()` 清 steer + followUp，**保留 nextTurn**；发
+   `session/aborted { clearedSteer, clearedFollowUp, nextTurnRetainedCount }` notification。
+ Pending `permission/request` 在 abort 时必须用 `Cancelled` outcome 响应
+   （ACP 0.12 硬约束，schema 行 728-735）。
```

**影响**：A3 §2-§8 全部、B1 EnginePhase、B6 reducer pending Map、B7 cancel propagation、C3 client cancel 全链路。

---

### 1.6 D-008: Subagent 无 wire `inherited_permissions` 字段

**来源**：A3 §7.3 / §9 Q5

**建议修订**：在 D-008 文本明示「Subagent 权限继承靠 `SubagentDefinition` 上 `Option` 字段缺省语义；wire 上**不存在** `inherited_permissions` 字段」。与 Claude Code SDK 行为对齐。

**影响**：A3 §7、B6 reducer 父子双调路径、B8 subagent spawn 输入草图。

---

### 1.7 D-012: reserved 5 个清单调整

**来源**：A4 顶警告 + §8 TODO A4-D1 / B9 TODO B9-2

**当前 D-012 原文**：14 必含 + 5 reserved（含 `WorktreeCreate / WorktreeRemove`）。

**建议修订**：
- **下沉 Phase 3**：`WorktreeCreate / WorktreeRemove`（与 zhive Phase 1 不相关，zhive 不做 git 集成）
- **新增 reserved**：
  - `PostCompact`（对偶 `PreCompact`；A4 提议）
  - `PreProviderRequest` / `PostProviderResponse`（A4 提议；B10 落地时挂钩点）
  - `PreBranchSummary` / `PostBranchSummary`（B9 提议；与 `zhive.branch_summary` span 配套）

**影响**：A4 §6 reserved 表、B5 hook host 注册表、B9 span 父子关系。

---

### 1.8 D-012: `ToolApprovalChange` 是否真在 14 必含

**来源**：A4 顶警告 + TODO A4-D2

**问题**：`ToolApprovalChange` 在 Claude Code 19 事件中**无对应**，最接近是 `PermissionRequest`（已独立）。

**建议修订**：D-012 修订时核对源出处，二选一：
- (a) 保留 `ToolApprovalChange` 为 zhive 自有事件（用于覆盖 user 手动 toggle permission_mode 的场景）
- (b) 替换为 Claude Code `MessageDisplay`

A4 当前 §6 暂保留 (a)。

---

### 1.9 D-012: `Setup` 是 Claude Code TS-only

**来源**：A4 顶警告 + TODO A4-D3

**问题**：D-012 把 `Setup` 列入 14 必含，但 Claude Code 文档将 `Setup` 标为 TypeScript-only。

**建议修订**：
- 若 Phase 1 不实装 `Setup` 触发逻辑（init / maintenance），改为 reserved
- 若 B1 engine loop 落地时确定要实装，保留 14 之一

由 B1 落地时回头敲定。

---

### 1.10 D-012: 新增第 15 事件 `PhaseTransition`

**来源**：B1 §6.7 / TODO B1-3 / B5 TODO B5-8

**建议修订**：在 14 之外新增 `PhaseTransition { from: EnginePhase, to: EnginePhase, thread_id: Option<ThreadId> }`。

**理由**：
- D-012 字面写"至少 14"和 `#[non_exhaustive]`，加新 case 不破坏决策
- 让 hook 能监测 EnginePhase 转换、metric/tracing 直接 span（D-014）
- 与 B9 `zhive.phase.{name}` 长 span 思路配套

A4 实装 `HookEvent` 时补这一 case。

---

### 1.11 D-013: 三 namespace 名修订（R-D013）

**来源**：A5 顶警告 + §10 R-D013

**当前 D-013 原文**：
> `kind: skill | slash_command | hook`

**建议修订为**：
> `kind: extension | prompt | skill`
> 其中 `extension` 内含子表 `[[tools]] [[hooks]] [[slash_commands]] [[shortcuts]] [[flags]]`

**理由**（A5 §3 开头三条）：
1. Pi 一手 namespace 名是 `extension | prompt | skill`（`rpc-types.ts:82`）
2. extension 本身就是 hook + tool + command + shortcut + flag + renderer 的**聚合容器**，把 hook / slash_command 拍平成 namespace 与 Pi 模型不一致
3. 拍平会拆散 manifest 的物理边界（同目录 extension.toml + commands/*.toml 并存，发现器要爬两层）

**影响**：A5 §3-§9 manifest schema 全部、B5 hook host 注册表、D-013 文本。

---

### 1.12 D-013: hook 必须挂 extension manifest（R-D013-2）

**来源**：A5 §10 R-D013-2

**建议修订**：D-013 文本明示「hook 必须挂 extension manifest，不允许 settings 顶层裸注册」。

**理由**：与红线 10（hook base 必带 `registered_by: ExtensionRef`）直接联动。settings 顶层裸注册无法填 `registered_by`，违反红线 10。

---

### 1.13 D-013: `entrypoint` 字段 Phase 1 仅承认 `"builtin"`（R-D013-3 隐含）

**来源**：A5 §2 表（字段 #10 execute 拒收）+ TODO A5-1

**建议修订**：D-013 明示 Phase 1 `entrypoint` 字段仅承认 `"builtin"`；第三方 extension code 运行能力（wasm / cmd exec）推到 Phase 2。

**理由**：
- D-005 不允许 in-core spawn extension
- Phase 1 仅 builtin hook 已能覆盖红线 11（mutate 后重验证）所有路径
- 第三方 extension 涉及 manifest entrypoint 字段语义 + subprocess executor 协议，需独立 Phase 2 deliverable

---

### 1.14 D-014: 新增 `zhive.compaction` + `zhive.branch_summary` 容器 span

**来源**：B9 TODO B9-1 / B9-2

**当前 D-014 原文**：仅要求覆盖 6 个 span（`Turn / Hook / Subagent / Permission / ToolCall / RollbackPoint`）。

**建议修订**：补两个容器 span，对应 EnginePhase 6 态中两个尚未覆盖的态：
- `zhive.compaction`（EnginePhase::Compaction 期间，覆盖 PreCompact + 内部 LLM 调用 + PostCompact）
- `zhive.branch_summary`（EnginePhase::BranchSummary 期间，覆盖 fork 准备 + LLM summary + RollbackPoint）

**理由**：D-014 字面写"至少 6 个 span"，加新 span 不破坏决策；OTel backend 按 span 时间轴可视化时能看到 phase 容器。

**影响**：B9 §2 span 矩阵、A4 reserved hook（与 §1.7 `PreBranchSummary / PostBranchSummary` 配套）。

---

## 2. 风险触发 / 待用户决策

### 2.1 R-2: rusqlite cold release build 78-80s（>60s 阈值）

**实测数据**（B3 §8 / 2026-05-28 测）：
- rusqlite =0.40 + bundled feature + 4 DB schema
- **cold release build：78-80s**（超过 plan §9 R-2 阈值 60s，未达 >2min 极端值）
- 二进制 +2.5MB
- 启动 1.5ms

**B3 缓解清单**（§8.4）：
- sccache（已部署）
- dev profile 日常开发（cold dev build < release）
- 关闭多余 feature

**待用户决策**：是否接受 release 慢路径？若要砍，备选：
- (a) 切系统 sqlite3 链接方案 —— Windows/macOS 用户体验劣化
- (b) 切 runtime download libsqlite3 —— 引入运行时依赖

**决策点**：✅ **接受**（用户拍板 2026-05-28 —— release 慢路径可接受，dev build 10-11s 不受影响）

---

### 2.2 R-7: connection pool 选型（B3 未自决，等用户拍）

**来源**：B3 §6 / TODO B3-2

**三方案对照**（B3 §6.1）：

| 方案 | 新依赖? | 优点 | 缺点 |
|---|---|---|---|
| a. r2d2-sqlite | ✅ 触发红线 1（2 dep） | 同步 API；社区主流 | 不与 tokio 异步亲和 |
| b. deadpool-sqlite | ✅ 触发红线 1（3 dep） | tokio-native；async API | 3 新 dep |
| c. 自写 mini pool | ❌ | 0 新 dep；可控 | 80-150 行；维护负担 |
| **d. 无 pool（`Arc<Mutex<Connection>>`）** | ❌ | 0 新 dep；最简；20 行；DB 串行写避免 SQLITE_BUSY | 写并发 = 0；读锁死（除非每库加 readonly connection） |

**B3 推荐方向**（**仅建议，等用户拍**）：
- **短期 Phase 1 起步**：方案 d（`Arc<Mutex<Connection>>` × 4 库，每库独立锁）
- **中期演进**：方案 c（自写 mini pool）或方案 b（走红线 1 审批）

**决策点**：✅ **方案 d** 落定 (2026-05-28)。Phase 1 单用户写入低频，串行可接受；如未来出现 SQLITE_BUSY 瓶颈再切方案 c 自写 mini pool。方案 b/c 触红线 1 不进 Phase 1。

---

### 2.3 R-8: 跨库一致性（已自决，方案已选）

**来源**：B3 §7 / B2 TODO B2-7

**结论**：✅ **已采纳** D-011 "JSONL source of truth + DB 异步重建" 路径。

**实现细节**：
- 写顺序硬约定：JSONL 先 append + fsync，DB 异步 catch-up
- 崩溃恢复：从 JSONL 重建 state.db / logs.db / memories.db 索引
- 接受 trade-off：崩溃时 state.db 可能落后 JSONL 几条；可从 JSONL 重建
- B3 §7.3 已落地崩溃恢复伪码

**待用户审阅**：若用户要求强一致，需引入 D-011 之外的方案（不在 R-8 缓解范围）。

---

### 2.4 R-3: tower-lsp 不再作为蓝本（已落槌）

**来源**：B4 §2 / §10

**结论**：✅ **R-3 已落槌**。
- tower-lsp 2022 最后 release / 2023-03 最后 commit，**已不代表"现代标杆"**
- 切换至 `async-lsp` 设计 + `codex` 工程双蓝本
- **不引入 async-lsp 作 dependency**（D-003 + 红线 1），仅作 design reference

**残余风险**：无重大。若 Phase 2+ 引入 middleware 体系，再评估。

---

### 2.5 R-4: llmsdk provider 适配（未触发）

**来源**：B10 §0 / §5

**结论**：✅ **R-4 未触发**。
- llmsdk trait 4 个必需能力（completion / stream / tool_call / reasoning）全覆盖
- 需要的「适配」纯发生在 `StreamPart -> Item` 的 fold 逻辑（B1 engine loop 实装）
- 不是 trait 设计问题

---

### 2.6 R-6: tracing-opentelemetry Phase 1 不装（部分触发）

**来源**：B9 TODO B9-6

**结论**：⚠️ **部分触发**。Phase 1 决定不装 `tracing-opentelemetry`，但字段名按 OTel semconv 预先对齐（避免后悔）。

**待用户决策**：
- (a) Phase 1 在 Cargo.toml 加 `tracing-opentelemetry` / `opentelemetry-sdk` 作为 optional dep（**触发红线 1**）
- (b) Phase 1 完全不在 Cargo.toml 提 OTel crate 名，feature gate 代码用 stub trait + 文档注释占位，Phase 2 装时再加 dep

**B9 推荐**：(b)，更严守红线 1。

**决策点**：✅ **方案 b** 落定 (2026-05-28)。Phase 1 无部署 OTel 需求；字段命名按 OTel semconv 预对齐确保 Phase 2 装 dep 时零 wire 改动。

---

### 2.7 红线 1 触发待审：新增 dependency

**B5 TODO B5-2**：`jsonschema = "0.18"` —— 红线 11 重验证（PreToolUse mutate `tool_input` 后强制重过 schema）必需。✅ **同意走 cargo add** (2026-05-28)。自写校验器风险高，jsonschema 0.18 社区主流。

**B5 TODO B5-3**：`futures` crate —— `catch_unwind` 对 async closure 支持。✅ **workspace 已有** (2026-05-28)：root `Cargo.toml:46` 已声明 `futures = { version = "0.3", default-features = false, features = ["std"] }`；zhive-core/zhive-client-native/zhive-tui 均 `{ workspace = true }` 引用。**不触红线 1**。

**B9 TODO B9-6**：见 §2.6。

---

### 2.8 红线 2 触发待审：unsafe

**B4 TODO B4-2**：`libc::getuid()` 为 unsafe（违反红线 2），用于 `/tmp/zhive-<uid>.sock` 回退路径。

**替代方案**：
- (a) `rustix` crate（成熟 safe wrapper，但属新增依赖触红线 1）
- (b) **仅依赖 `XDG_RUNTIME_DIR`，无则报错让用户配置**（B4 推荐）

**决策点**：✅ **方案 b** 落定 (2026-05-28)。强制 `XDG_RUNTIME_DIR` 存在，缺失时启动报错让用户配置；避免 unsafe 调用与新增依赖。

---

## 3. wire schema 新增（不算决策修订，但影响 zhive-proto）

> 本节列出 19 个 deliverable 期间新增的 wire surface。不破坏 D-XXX 已锁部分，但需要 zhive-proto 落地。

### 3.1 A3 / B6 三队列模型相关

- `session/next_turn` request（独立 RPC method，驱动 NextTurn 队列，**不在** `streamingBehavior` 二元枚举内）
- `session/aborted` notification（payload 含 `cleared_steer / cleared_follow_up / next_turn_retained_count`）
- `PermissionOutcome::Cancelled` variant（ACP 0.12 硬约束 verbatim）
- `PermissionOutcome::Defer { reason: Option<String> }` variant（zhive 扩展，触发 turn 挂起）

### 3.2 B6 permission Defer 续命相关

- `session/resume_permission` request（client → server，续命已 defer 的 permission request）
- `ResumeOutcome { Selected { option_id } | Cancelled }`（resume 时**不可再次 Defer**，schema 强制）
- `turn/suspended` notification（server → client；defer 触发，payload 含 `request_id / suspended_at / reason?`）
- `turn/resumed` notification（server → client；resume 后续 turn）

### 3.3 B1 engine 状态机相关

- `phase/changed` notification（method 名）+ `PhaseChangedNotification { from, to, thread_id }` payload（A1 草图中缺，B1 §4 末尾补）

### 3.4 C4 反向 method 表修订

- `permission/request` → 改为 ACP 字面 **`session/request_permission`**（修订 C1 §6.3 第一行）
- ~~`hook/run`~~ **删除**（hook dispatch 是 server 内部，不走反向 RPC；修订 C1 §6.3 第二行）
- `session/request_user_input` 保留（zhive 私有反向 method，未来 Phase 2 评估提交 ACP 标准）

### 3.5 B9 tracing span（与 D-014 配套）

- 容器 span：`zhive.compaction` / `zhive.branch_summary`（见 §1.14）
- RPC 层 span（建议 B4 在 server dispatcher 入口）：含 `rpc.method` / `jsonrpc.request.id` —— 是否进 D-014 必覆盖清单待决（TODO B9-4）

---

## 4. Phase 1 不实装但占接口的项

> 以下 6 项 Phase 1 决定保留接口位但不实装运行时；用户审阅是否同意。

| # | 项 | 来源 | 占位形态 |
|---|---|---|---|
| 1 | Windows lockfile + 127.0.0.1 transport | B4 §3 / TODO B4-3 | CLI flag `--transport windows-lockfile` 占位，运行时报 `TransportNotImplementedInPhase1` 错误 |
| 2 | client 自动重连（Disconnected 终态） | C2 §4 / TODO C2-N1 | `ClientBuilder::reconnect_policy(...)` 不暴露；caller 走 `connect_*()` 重建 |
| 3 | ACP 0.12 reserved 6 个反向 method（`fs/read_text_file` / `fs/write_text_file` / `terminal/create` / `terminal/output` / `terminal/release` / `terminal/wait_for_exit` / `terminal/kill`） | C4 §4 反向 method × 默认行为表 | 未注册时默认 `JsonRpcError::method_not_found`，server 收到 `-32601` 后降级 |
| 4 | tracing-opentelemetry layer | B9 §5.3 / TODO B9-6 | `#[cfg(feature = "otel")]` gate；Phase 1 不加 Cargo.toml dep（走 stub trait） |
| 5 | Subprocess hook executor | B5 §0 摘要 + TODO B5-9 | `HookExecutor { InProcess(BoxedHookFn), Subprocess(...) }` 枚举保留双轨；Phase 1 仅实装 `InProcess` variant |
| 6 | Extension 热重载 trigger | A5 §7 + TODO A5-7 | scope token (`ExtensionScope` + `HookHandle`) 已就位；Phase 1 不暴露 `/reload` CLI 入口，但 host 端 `unregister_scope` 已存在 |

---

## 5. 未决项总计（TODO 汇总）

> 19 个 deliverable 内所有 `TODO(开放项 ...)` / `TODO(B5-...)` / `TODO(开放项 X-N...)` 行数统计。

| Deliverable | TODO 数 | 关键 TODO（涉及决策 / 红线 / 跨任务） |
|---|---|---|
| A1 | 10 | **OP-1**（D-006 8→14 case 扩展，本表 §1.1）；OP-2 ACP 未知 case 降级到 SystemNotice；OP-3 MCP 多 tool_call 新 Turn 语义 |
| A2 | 13 | **A2.1**（D-007 双命名空间重定义，§1.3）；**A2.3**（initialized notification 纳入，§1.4）；A2.4 cancellation 默认 true 不对称 |
| A3 | 11 | **三队列模型词条修订**（§1.5）；**A3-O3** B6 落定 permission/request server 侧 timeout 30s（B6 已采纳）；A3-O4 BypassPermissions 短路（B6 已采纳） |
| A4 | 13 | **A4-D1 / D2 / D3**（D-012 reserved 调整 + ToolApprovalChange + Setup，§1.7-1.9）；A4-S1 PermissionScope typed（A3 收敛后） |
| A5 | 10 | **R-D013 / R-D013-2 / R-D013-3**（D-013 三 namespace 修订 + manifest-only hook + entrypoint Phase 1，§1.11-1.13） |
| B1 | 9 | **B1-3**（PhaseTransition hook 第 15 事件，§1.10）；B1-1 Retry 独立 phase；B1-2 per-thread phase 字典 |
| B2 | 8 | B2-7 跨库 sync 一致性窗口（已纳 R-8 方案）；B2-8 subagent 子 ThreadHandle 同 engine |
| B3 | 8 | **B3-4 R-2 用户拍**（§2.1）；**B3-2 R-7 pool 选型**（§2.2）；B3-1 memories.jsonl 是否落盘；B3-5 zhive-core 单 crate 编译时间 |
| B4 | 12 | **B4-2 红线 2 unsafe**（§2.8）；B4-3 Windows transport 占位；B4-4 ClientRequest 放 proto 还是 core |
| B5 | 9 | **B5-2 jsonschema dep**（§2.7）；B5-3 futures dep；B5-7 串行 vs 并行分歧；B5-8 PhaseTransition 第 15 事件 |
| B6 | 7 | B6-O3 30s timeout（已采纳）；B6-O6 child Defer 父子两层 suspended；B6-O7 Cancelled internally tagged vs ACP variant |
| B7 | 7 | B7-3 compaction cancel 公开 API 形态；B7-4 PendingSessionWrites flush 失败不回填 |
| B8 | 9 | B8-1 subagent_decision/final 双 channel 是否合并；B8-3 child ThreadHandle 保留；B8-5 background:true 拒收 |
| B9 | 7 | **B9-1 / B9-2**（zhive.compaction / branch_summary 容器 span，§1.14）；**B9-6 OTel 红线 1**（§2.6）；B9-3 phase span 利弊 |
| B10 | 6 | B10-1 Item::ToolCall.kind 填充时机；B10-5 provider 配置 / API key 注入路径 |
| C1 | 7 | C1-N1 method 常量表；C1-N2 多 handler 串联；C1-N5 双反向入口 |
| C2 | 5 | C2-N1 Phase 2/3 自动重连设计草案；C2-N2 child 退出 exit_status 暴露；C2-N4 reverse-RPC pending 断连 Cancelled |
| C3 | 6 | C3-N1 cancel unknown sid 静默忽略；C3-N3 shutdown 是否先发 cancel；C3-N5 cancel × handler 跑完语义 |
| C4 | 8 | **C4-N2 C1 §6.3 修订**（§3.4）；C4-N5 session/request_user_input 是否提交 ACP；C4-N6 panic message 不暴露 |
| **合计** | **172** | 含 ~30 项决策修订 / 红线 / 跨任务关键 TODO |

> 注：B5 含 9 个 `TODO(B5-N)` 行（§10 汇总 + mid-doc 重复列出）。总数按 unique 计 9。

---

## 6. 调研完成的反向检验（plan §7.4 退出条件）

**Plan §7.4 退出条件**：「能不能给一个**没看过 5 个 repo 的工程师**直接派活写 `Engine::run_turn()`？如果不能，回到对应 deliverable 补」。

**回答**：✅ **yes — 可以派活**（partial: 7 处需用户拍）

**支撑证据**（B1 deliverable）：
- B1 §4 已落地 `Engine` 公开 method 签名 + `Submission` enum + `EngineEvent` enum + `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` + `ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>`
- B1 §6 channel 拓扑（broadcast(1024) / mpsc(512) / oneshot / watch）+ 锁层级
- A1 §2.3 Turn boundary 算法伪码（`on session/prompt → start_turn → spawn agent_loop → emit turn/started / item-level notifications / turn/completed`）
- A3 三队列 + B7 cancel propagation：steer / followUp drain 时序闭合
- B6 reducer 父子双调 + pending_approvals Map lifecycle
- B5 hook host 串行 dispatch + 红线 11 重验证
- B10 provider fold `StreamPart -> Item` 路径

**`Engine::run_turn()` 工程师能做到**：
1. ✅ 读 B1 §4 看签名 → 写出 `pub async fn start_turn() -> Result<TurnId, EngineError>` skeleton
2. ✅ 读 A1 §2.3 + B1 §6 → 实装 agent_loop 主循环（外层 follow-up + 内层 tool_call/steering）
3. ✅ 读 B5 + B6 → 接入 hook + permission reducer
4. ✅ 读 B7 → 实装 cancel propagation + nextTurn 保留
5. ✅ 读 B10 → 调 `provider.do_stream(CallOptions) -> StreamResult`，fold 到 Item
6. ✅ 读 B3 §7 + B2 §4 → 写入 JSONL（source of truth）+ state.db 异步索引

**partial 缺口（已全部消除，2026-05-28 终审）**：
1. ✅ R-2：rusqlite cold release 78-80s 已接受
2. ✅ R-7：connection pool **方案 d**（`Arc<Mutex<Connection>>` × 4 库）落定
3. ✅ D-006 8→14 case 采纳
4. ✅ D-008 三队列修订采纳
5. ✅ D-012 第 15 事件 PhaseTransition 采纳
6. ✅ D-013 三 namespace 修订（R-D013）采纳
7. ✅ 红线 1 / 红线 2：jsonschema 走 cargo add；futures 用现有 workspace dep；rustix 不引入；opentelemetry 走 stub trait（Phase 2 再装）

**结论**：Phase 1 调研产出 ✅ **足以派活，且决策已全部定稿**。可以直接启动 Phase 1 落地 PR 序列。落地者只需按 §1-§4 各节"决策点"字段对应实装即可，无需回头改 wire schema / Cargo.toml dep / decision 原文（同步 `research/99-decisions/README.md` 在落地 PR 序列启动前一次完成）。

---

## 7. 终审完成情况（2026-05-28）

1. **第一轮 - 决策修订**（§1 共 14 条 D-XXX 修订）：
   - ✅ **全部 14 条采纳**（含 D-012 `ToolApprovalChange` 保留方案 (a) / `Setup` 改 reserved）
   - 待办：把采纳的修订一次性同步到 `research/99-decisions/README.md`（落地 PR 序列启动前完成）

2. **第二轮 - 风险触发 & 红线触发**（§2 共 7 项）：
   - ✅ R-2：rusqlite cold release 78-80s 接受
   - ✅ R-7：connection pool 走方案 d（`Arc<Mutex<Connection>>` × 4 库，0 新 dep）
   - ✅ R-8：JSONL source of truth 已选
   - ✅ R-3 / R-4：已落槌（tower-lsp 不作蓝本 / llmsdk 全覆盖）
   - ✅ R-6：tracing-opentelemetry 走方案 b（stub trait + 文档占位）
   - ✅ 红线 1：jsonschema 走 cargo add；futures 用现有 workspace dep
   - ✅ 红线 2：B4 强制 `XDG_RUNTIME_DIR`，取消 `libc::getuid()` fallback

3. **第三轮 - wire schema + Phase 1 占位**（§3 / §4）：
   - §3 wire 新增项随 zhive-proto crate 落地 PR 实装（无需单独决策）
   - §4 6 项 Phase 1 不实装 → ✅ 全部确认保留接口位

4. **第四轮 - 启动 Phase 1 落地**：
   - 调研已通过 §6 反向检验
   - **所有决策已定稿**，可按 plan §10 启动落地 PR 序列

---

## 附录 A: 19 个 deliverable 索引

| # | Deliverable | 行数 | 主交付 |
|---|---|---|---|
| 1 | A1-thread-turn-item.md | 804 | Item 14 case + Thread/Turn schema + bridge 桥接表 |
| 2 | A2-initialize-capabilities.md | 274 | initialize 握手 + Capabilities 字段 + protocolVersion u16 |
| 3 | A3-permission-streaming-subagent.md | 691 | Permission 四态 + 三队列模型 + Subagent 继承 |
| 4 | A4-hook-event-schema.md | 926 | HookEvent 14 case + HookEventBase + Unknown 兜底 |
| 5 | A5-extension-manifest.md | 515 | Manifest 三 namespace（R-D013）+ 12 字段处理表 |
| 6 | B1-engine-loop.md | 872 | Engine actor + EnginePhase 6 态 + Submission/Event 拓扑 |
| 7 | B2-state-memory-model.md | 501 | 活跃 Thread/Turn/Item 内存表征 + lazy load |
| 8 | B3-persistence.md | 782 | rusqlite 4 库 + JSONL Leaf rollout + R-2/R-7/R-8 |
| 9 | B4-server-transport.md | 657 | JSON-RPC over stdio + UDS + R-3 落槌（tower-lsp 不再蓝本）|
| 10 | B5-hook-host.md | 908 | Hook host 串行 + ExtensionScope + 红线 10/11 落地 |
| 11 | B6-permission-reducer.md | 518 | Reducer 接入 Engine + Defer 续命 + 父子双调 |
| 12 | B7-cancel-streaming.md | 618 | Cancel propagation + 三队列状态机 + pendingSessionWrites |
| 13 | B8-subagent.md | 455 | Subagent fresh window + only final + 禁递归 |
| 14 | B9-tracing.md | 340 | 6+2 span（含 §1.14 新增）+ OTel semconv 字段命名 |
| 15 | B10-provider.md | 311 | llmsdk LanguageModel trait 复用 + R-4 未触发 |
| 16 | C1-client-api.md | 563 | Client API + ReverseHandler trait + builder |
| 17 | C2-reconnect.md | 268 | 连接 lifecycle + Phase 1 不做自动重连 |
| 18 | C3-client-cancel.md | 341 | Client cancel 路径 + 与 server 协议对齐 |
| 19 | C4-reverse-handler.md | 453 | Reverse handler dispatch + method 表 + C1 §6.3 修订 |
| **合计** | | **10797 行** | |

---

## 附录 B: deliverable 顶部"决策冲突警告" / "设计衔接警告"全清单

| Deliverable | 类型 | 内容摘要 |
|---|---|---|
| A1 §2.1 | 决策冲突警告 | codex 用 `tag="type"` / ACP 用 `tag="sessionUpdate"` / rmcp 用 `tag="type"`；zhive 选 `kind`，理由 §1.2 |
| A3 顶 | 决策冲突警告 | D-008 二元 → 三队列修订（§1.5） |
| A4 顶 ×3 | 决策冲突警告 | `ToolApprovalChange` 在 Claude Code 无对应（§1.8）；`Setup` 是 TS-only（§1.9）；reserved 5 个调整建议（§1.7） |
| A5 顶 | 决策冲突警告 | D-013 三 namespace 修订（R-D013，§1.11） |
| B1 §0 | 设计衔接警告 | A1 TurnStatus 4 态 ⊥ B1 EnginePhase 6 态正交共存，**不改 A1** |
| B3 §0 | 风险触发 | R-2 触发（cold release 78-80s）+ R-7 等用户拍 + R-8 fail-strategy |
| B5 顶 ×3 | 决策冲突警告 | Claude Code 并行 vs zhive 串行（§1 / §2.7）；A4 PreToolUse mutate 后 abort turn（不回滚）；B1 PhaseTransition 待 A4 修订 |
| B7 顶 | 决策衔接警告 | 继承 A3 三队列；R5 finding #1 应回头修订 |
| B8 顶 | 设计衔接警告 | child engine = 同 EngineInner.threads 内新 ThreadHandle（非新 Engine 实例）|
| B9 顶 | 设计衔接警告 | D-014 6 span + 本 deliverable 补 §1.14 容器 span |
| C2 顶 | 范围声明 | Phase 1 不做自动重连（Disconnected 终态，caller 走 connect_*() 重建）|
| C4 §4.3 | 决策修订建议 | C1 §6.3 反向 method 表 3 条修订（§3.4）|
