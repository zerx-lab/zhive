---
task: A2
title: initialize 握手 + capabilities 协商（D-007 落地）
date: 2026-05-28
status: draft
depends_on:
  - research/99-decisions D-007 (强制 initialize 握手 + v1/v2 命名空间 + capabilities 协商)
  - research/99-decisions D-005 (rmcp/ACP 仅在 bridge crate)
  - research/99-decisions D-006 (serde+schemars 单 schema 源)
references:
  - ${CODEX}/app-server-protocol/src/protocol/v1.rs                          (InitializeParams / ClientInfo / InitializeCapabilities / InitializeResponse 全部住在 v1 模块)
  - ${CODEX}/app-server-protocol/src/protocol/common.rs                     (client_request_definitions! 宏 + `Initialize` 变体 + 序列化测试黄金 JSON)
  - ${CODEX}/app-server-protocol/src/lib.rs                                 (`pub use protocol::v1::Initialize{Params,Response,Capabilities}` re-export)
  - agent-client-protocol-schema 0.12.0/src/agent.rs                       (InitializeRequest / InitializeResponse / AgentCapabilities / PromptCapabilities / McpCapabilities / AGENT_METHOD_NAMES / INITIALIZE_METHOD_NAME)
  - agent-client-protocol-schema 0.12.0/src/client.rs                      (ClientCapabilities / FileSystemCapabilities / terminal flag)
  - agent-client-protocol-schema 0.12.0/src/version.rs                     (ProtocolVersion(u16) + V0 fallback + Deserialize 兼容 string→V0)
  - crates/zhive-proto/src/lib.rs                                          (ErrorObject { code, message, data } 已就位)
---

> 范围声明：本 deliverable 仅为 A2 子任务调研产出；**不**包含任何 zhive crate 实现代码改动。
> ACP 路径均指 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/agent-client-protocol-schema-0.12.0/`，被 `agent-client-protocol = "=0.12.1"`（D-005）传递依赖。
> codex 路径均指 `~/Desktop/code/github/codex/codex-rs/`。
> **重要前提**：codex 没有把 `Initialize` 放进 `protocol/v2/` 子目录；`v2` 仅承载 thread/turn/item 等新原语，而握手类型留在 `v1.rs`，通过 `protocol::v1::InitializeParams` 在 `common.rs` 的 `client_request_definitions!` 宏中被引用。**zhive v1/v2 命名空间共存的证据未直接定位**——本 deliverable 关于 zhive 双命名空间编码规则的结论基于 plan §4 A2 + codex `client_request_definitions!` 宏的字面行为推断。

---

## 1. 参考点清单

| 论断主题 | 路径 | 行号 |
|---|---|---|
| codex `InitializeParams { client_info, capabilities: Option<InitializeCapabilities> }` | `${CODEX}/app-server-protocol/src/protocol/v1.rs` | 27-33 |
| codex `ClientInfo { name, title, version }` | `${CODEX}/app-server-protocol/src/protocol/v1.rs` | 35-41 |
| codex `InitializeCapabilities { experimental_api, request_attestation, opt_out_notification_methods }` —— **三个 bool/list flag，无 protocolVersion 字段** | `${CODEX}/app-server-protocol/src/protocol/v1.rs` | 43-57 |
| codex `InitializeResponse { user_agent, codex_home, platform_family, platform_os }` —— **响应里没有 server capabilities，也没有 protocolVersion** | `${CODEX}/app-server-protocol/src/protocol/v1.rs` | 59-71 |
| codex `client_request_definitions!` 宏头：`$variant $(=> $wire:literal)? { ... }` —— wire 字符串可选；缺省时变体名走 `rename_all = "camelCase"` | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 161-189 |
| codex `Initialize { params, response }` —— **缺 `=> "..."` 箭头**，故 wire method = `"initialize"`（camelCase 默认） | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 435-440 |
| codex 序列化测试：`"method": "initialize"` 黄金 JSON | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 2008-2030 |
| codex 反序列化测试：同样 `"method": "initialize"` | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 2034-2076 |
| codex re-export：`pub use protocol::v1::Initialize{Capabilities,Params,Response}` 在 crate 根 | `${CODEX}/app-server-protocol/src/lib.rs` | 34-36 |
| codex `Initialized` notification 变体（client→server 完成握手通知） | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 1563-1565 |
| codex `Initialized` 序列化测试 `"method": "initialized"` | `${CODEX}/app-server-protocol/src/protocol/common.rs` | 2102-2107 |
| ACP `InitializeRequest { protocol_version, client_capabilities, client_info, meta }` | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 46-71 |
| ACP `InitializeRequest::new(protocol_version)` builder | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 73-108 |
| ACP `InitializeResponse { protocol_version, agent_capabilities, auth_methods, agent_info, meta }` | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 110-147 |
| ACP `Implementation { name, title, version, meta }`（≈ codex `ClientInfo`） | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 197-220 |
| ACP `AgentCapabilities { load_session, prompt_capabilities, mcp_capabilities, session_capabilities, ... }` —— **嵌套对象不是单 bool** | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 3865-3928 |
| ACP `PromptCapabilities { image, audio, embedded_context, meta }` 嵌套 | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 4395-4424 |
| ACP `McpCapabilities { http, sse, meta }` 嵌套 | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 4468-4487 |
| ACP `ClientCapabilities { fs: FileSystemCapabilities, terminal: bool, ... }` —— **混合：fs 嵌套 + terminal 单 bool** | `agent-client-protocol-schema-0.12.0/src/client.rs` | 1505-1568 |
| ACP `INITIALIZE_METHOD_NAME: &str = "initialize"`（无前缀，无 namespace） | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 4653 |
| ACP `AGENT_METHOD_NAMES.initialize = INITIALIZE_METHOD_NAME` | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 4604-4605 |
| ACP `AgentRequest::InitializeRequest(_) → AGENT_METHOD_NAMES.initialize` | `agent-client-protocol-schema-0.12.0/src/agent.rs` | 4899 |
| ACP `ProtocolVersion(u16)` + `V0`/`V1`/`LATEST` 常量 | `agent-client-protocol-schema-0.12.0/src/version.rs` | 9-33 |
| ACP `ProtocolVersion` Deserialize：u64→u16 严格；string→V0 兼容旧 semver | `agent-client-protocol-schema-0.12.0/src/version.rs` | 37-83 |
| ACP doc-comment：「This version is only bumped for breaking changes. Non-breaking changes should be introduced via capabilities.」 | `agent-client-protocol-schema-0.12.0/src/version.rs` | 5-8 |
| zhive `ErrorObject { code: i64, message, data }` 已就位 | `crates/zhive-proto/src/lib.rs` | 174-181 |

> TODO(开放项-A2.1)：codex `protocol/v2/` 目录下 28 个文件均未提及 `Initialize`；`v2` 命名空间是否有独立握手类型未直接定位。本 deliverable 假设 codex 的「双命名空间共存」表现为：**同一个 `initialize` 入口 + `capabilities.experimental_api: bool` 单 flag 控制是否启用 v2 方法**，而非两个不同的 `vN/initialize` 方法字符串。
> TODO(开放项-A2.2)：ACP 端版本协商失败的具体错误码未在 ACP schema crate 里发现常量（agent.rs error 模块、`error.rs`、`elicitation.rs` 内 grep `INITIALIZE` / `VERSION` / `negotiation` 均无明文）。需查 ACP 主 crate `agent-client-protocol = 0.12.1` 而非 schema sub-crate；本次按 ACP 文档常见做法 + zhive ErrorObject.code 设计推断。

---

## 2. `Initialize{Request,Response}` 类型草图

> 字段名采用 ACP 风格（`protocolVersion` / `clientCapabilities` / `serverCapabilities` / `clientInfo` / `serverInfo`），与 ACP 完全对齐；同时把 codex 的 `experimentalApi` / `requestAttestation` 等局部 flag 收编进 `Capabilities`。

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// wire method = `"initialize"`（与 ACP、codex 一致；无命名空间前缀）
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InitializeRequest {
    /// 单调递增 u16（ACP 风格）。**zhive 选 integer 而非 semver**——详见 §6 Q1。
    pub protocol_version: ProtocolVersion,
    /// 客户端声明的能力（请求侧），见 §3 字段表。
    #[serde(default)]
    pub client_capabilities: Capabilities,
    /// 客户端身份（aligns with ACP `Implementation` / codex `ClientInfo`）。
    pub client_info: Implementation,
    /// 扩展通道（ACP `_meta` 对齐）。
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InitializeResponse {
    /// 服务端选定的协议版本（≤ 请求侧 protocol_version；超出时返回错误，见 §6 Q1）。
    pub protocol_version: ProtocolVersion,
    /// 服务端声明的能力（响应侧），结构与 client_capabilities 同 schema。
    #[serde(default)]
    pub server_capabilities: Capabilities,
    /// 服务端身份。
    pub server_info: Implementation,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Implementation {
    /// 程序可读 ID，如 `"zhive-cli"`、`"zed"`、`"codex_vscode"`。
    pub name: String,
    /// 可选 UI 显示名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 实现版本（semver 字符串，如 `"0.1.0"`）。
    pub version: String,
}

/// 单调 u16；遵循 ACP `ProtocolVersion`。zhive 自身从 `V1` 开始（V0 保留为兼容标记）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    pub const V0: Self = Self(0); // 解析失败兜底，仅 deserialize 时产出
    pub const V1: Self = Self(1);
    pub const LATEST: Self = Self::V1;
}
```

> wire method 字符串：`"initialize"`（请求） / `"initialized"`（client→server notification，对齐 codex `ClientNotification::Initialized` 与 LSP 习惯；ACP 没有显式 `initialized` notification）。
> TODO(开放项-A2.3)：是否纳入 `initialized` notification（codex 有，ACP 无）尚未在 D-007 里固化；建议**保留**——理由：与 codex 对齐，给握手收尾一个明确的「双方就绪」信号。

---

## 3. `Capabilities` struct 字段表（≥ 6 个 flag）

**zhive 决策**：采用**简单 flag**（`bool` 或 `Option<bool>`）作为顶层，**嵌套对象仅用于多子能力分组**（如 `streaming`）。详见 §6 Q2。

| flag / 字段 | 类型 | 默认 | zhive 决策 | 对齐源 | 说明 |
|---|---|---|---|---|---|
| `hooks` | `bool` | `false` | **简单 flag** | D-008 / Claude Code Hooks | 客户端是否实现 `hook/run` 反向 RPC 与 `permission/request`。 |
| `subagents` | `bool` | `false` | **简单 flag** | D-008（R4 subagent 权限继承）| 客户端能否承载 subagent fanout / 继承 `PermissionScope`。 |
| `streaming` | `StreamingCapability` | `default()` | **嵌套对象**（一个例外）| Pi `streamingBehavior` + D-008 | 子字段 `{ steer: bool, follow_up: bool, next_turn: bool }`——同时承载 Pi 三队列模型的能力分级。 |
| `cancellation` | `bool` | `true` | **简单 flag** | LSP / ACP `session/cancel` 习惯 | 客户端实现 `turn/cancel`（`turn/interrupt`）反向支持。**默认 true**：取消是基线能力。 |
| `permission` | `bool` | `false` | **简单 flag** | D-008 reducer | 客户端实现 `permission/request` 反向 RPC（区别于 `hooks`：hooks 走 Claude Code 形状，permission 是裸 RPC）。 |
| `extension` | `bool` | `false` | **简单 flag** | D-010 / Claude Code Skills | 客户端是否实现 `extension/list` / `extension/load`，能承载 SDK Skills 形状。 |
| `experimental_api` | `bool` | `false` | **简单 flag** | codex v1 `experimental_api` | 启用 `#[experimental]` 标注的 v2 实验方法。**zhive 复用 codex 同义字段名**，避免双 namespace 在 wire 上分裂（见 §5）。 |
| `opt_out_notification_methods` | `Option<Vec<String>>` | `None` | **保留** | codex v1 | 客户端可声明丢弃指定 notification（带宽 / 噪声治理）。 |
| `_meta` | `Option<Value>` | `None` | **保留** | ACP | 扩展通道，未来无破坏性新增字段的预留位。 |

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Capabilities {
    #[serde(default)] pub hooks: bool,
    #[serde(default)] pub subagents: bool,
    #[serde(default)] pub streaming: StreamingCapability,
    #[serde(default = "default_true")] pub cancellation: bool,
    #[serde(default)] pub permission: bool,
    #[serde(default)] pub extension: bool,
    #[serde(default)] pub experimental_api: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_out_notification_methods: Option<Vec<String>>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StreamingCapability {
    #[serde(default)] pub steer: bool,
    #[serde(default)] pub follow_up: bool,
    #[serde(default)] pub next_turn: bool,
}

fn default_true() -> bool { true }
```

> TODO(开放项-A2.4)：`cancellation` 默认 `true` 与其他 flag 默认 `false` 不对称，是否破坏 `#[derive(Default)]` 语义需在 A3 联动 review。

---

## 4. 与 ACP `Initialize` 字段对齐表

| zhive 字段 | ACP 字段 | 对齐状态 | 说明 |
|---|---|---|---|
| `InitializeRequest.protocol_version: u16` | `InitializeRequest.protocol_version: ProtocolVersion(u16)` | ✅ 对齐 | 同 u16；同 V0 fallback 习惯（ACP version.rs L64-78）。 |
| `InitializeRequest.client_capabilities` | `InitializeRequest.client_capabilities: ClientCapabilities` | ⚠️ 改名 + 重构 | 字段名相同，但**内容不同**：ACP 用 `fs/terminal/auth/nes` 走文件系统视角；zhive 走 hooks/subagents/streaming 走 agent 引擎视角。**bridge crate（D-005）负责双向翻译**。 |
| `InitializeRequest.client_info: Implementation` | `InitializeRequest.client_info: Option<Implementation>` | ⚠️ 改名 | ACP 是 `Option`（向前兼容旧客户端）；zhive 强制必填（D-007 强制握手语义）。 |
| `InitializeRequest._meta` | `InitializeRequest.meta` (`#[serde(rename = "_meta")]`) | ✅ 对齐 | 同 `_meta` wire name。 |
| `InitializeResponse.protocol_version` | `InitializeResponse.protocol_version` | ✅ 对齐 | 同 u16。 |
| `InitializeResponse.server_capabilities` | `InitializeResponse.agent_capabilities` | ⚠️ 改名 | ACP 叫 `agentCapabilities`，zhive 叫 `serverCapabilities`——理由：zhive 不强制服务端是 agent（也可能是 bridge / proxy），且与 LSP `serverCapabilities` 对齐降低生态学习成本。`bridge` 层做字段重命名。 |
| `InitializeResponse.server_info` | `InitializeResponse.agent_info` | ⚠️ 改名 | 同上。 |
| `InitializeResponse.auth_methods` | `InitializeResponse.auth_methods: Vec<AuthMethod>` | ❌ 拒收 | zhive Phase 1 不做 auth 协商（D-007 范围外）。bridge 接收时丢弃；未来通过 `meta.auth` 临时承载。 |
| (无) | `AgentCapabilities.load_session` | ❌ 拒收 | zhive 走 `Thread/Turn/Item` 三层（D-006），无 ACP `session/load` 同义动作；bridge 把 `load_session=true` 映射到 zhive `thread/resume` 能力声明（隐式）。 |
| (无) | `AgentCapabilities.prompt_capabilities.{image,audio,embedded_context}` | ❌ 拒收 | zhive 不在 capabilities 层暴露 content kind 支持；改用 Item schema（D-006 / A1）自描述 `kind`。 |
| (无) | `AgentCapabilities.mcp_capabilities.{http,sse}` | ❌ 拒收 | zhive Phase 1 仅 stdio MCP（D-003 去 ConnectRPC + D-005 bridge）；mcp transport 协商不进 capabilities。 |
| `Capabilities.hooks / subagents / streaming / permission / extension / experimental_api` | (无) | ⚠️ zhive 独有 | ACP schema 里没有同义概念（ACP 用 `session_capabilities` 包装 mode 切换，但与 zhive hooks/permission 模型不重叠）。 |

> TODO(开放项-A2.5)：`session_capabilities` (ACP agent.rs L3883) 内含哪些子字段未在本次时间预算内深挖；A3 (permission/streaming) 子任务若发现 ACP `SessionCapabilities` 包含 mode/permission 相关字段，需回头补 alignment。

---

## 5. v1 / v2 命名空间在 method 字符串里的编码规则

### 5.1 codex 实测

- **wire 上没有 `v1/` / `v2/` 前缀**。codex 把 v1/v2 区分留在 **Rust 模块路径**（`protocol::v1::InitializeParams` vs `protocol::v2::ThreadStartParams`），不进 JSON method 字符串。
- 实测 method 字符串（从黄金 JSON 抽取）：
  - `"initialize"`（无前缀，来自 `Initialize` 变体缺省 camelCase；锚 common.rs L2010）
  - `"thread/start"`（显式 `=> "thread/start"`；锚 common.rs L445）
  - `"thread/resume"` / `"thread/fork"` / `"thread/archive"` 等（均显式标 wire 字符串）
- 「v1/v2 共存」的实际编码方式 = **`capabilities.experimentalApi: bool`** 单 flag 开关：v1 默认可见，v2 实验 API 通过 capability 才暴露（`#[experimental("thread/increment_elicitation")]` 等标注）。

### 5.2 zhive 最终建议

| 场景 | wire 格式 | 范例 | 理由 |
|---|---|---|---|
| v1 稳定方法 | `"<group>/<verb>"` | `"thread/start"` / `"turn/cancel"` / `"initialize"` | 与 codex / ACP / LSP 全对齐；最短表面积。 |
| v2 实验方法 | `"<group>/<verb>"` + capability gate | `"hook/run"`（需 `capabilities.hooks=true`） | wire 上**不加** `v2/` 前缀；通过 `experimental_api` 或具体 capability flag 让服务端按 `MethodNotFound` 拒绝。 |
| 长期破坏性升级 | bump `protocolVersion` → 整型 +1 | V1 → V2 | 同 ACP doc-comment「only bumped for breaking changes」。 |

**与 codex 实测对比**：✅ **完全一致**。zhive 不引入额外 `vN/` 前缀，保留 method 字符串的 grouped path 形态。**v1/v2 在 wire 上是隐式的，由 `protocolVersion` + `capabilities` 双键共同表征**。

> TODO(开放项-A2.6)：plan §4 A2 标题写「v1/v2 命名空间共存」一词容易被理解为 wire 前缀；本 deliverable 把该词重定义为「源码模块层 v1/v2 + wire 层 protocolVersion 整型 + capability flag」三件套。决策原文若需调整措辞需回到 D-007 修订。

---

## 6. 关键问题 3 条逐条作答

### Q1：`protocolVersion` 用 semver 还是整数？协商失败错误码？

**答**：**整数（u16）**，与 ACP `ProtocolVersion(u16)` 一致（version.rs L10）。

- ACP 走 u16，codex 干脆不暴露 protocolVersion（用 capability flag 替代）。两者都未走 semver。
- 优势：单调可比较（`Ord` 自动）；服务端拿请求侧版本 `min(server_latest, request)` 即可；JSON-schema 友好。
- ACP 兼容旧 semver string：deserialize 时 string→V0（version.rs L64-78）。zhive 复用同策略。
- **协商失败错误码**：使用 zhive-proto `ErrorObject.code: i64`，建议 `-32000`（JSON-RPC server-defined 起始）+ 域内编号：
  - `-32001  ProtocolVersionUnsupported`（请求版本 > server LATEST，且无法协商）
  - `-32002  CapabilityRequired`（请求声明但 server 未实现的 capability）
  - 错误 `data` 字段携带 `{ supported: [V1, V2], requested: V99 }`

### Q2：`capabilities` 编码方式：简单 flag vs 嵌套对象？

**答**：**简单 flag 优先 + 必要时嵌套对象**。

- ACP 实测是**混合**：顶层 `AgentCapabilities` 既有 `load_session: bool` 又有 `prompt_capabilities: PromptCapabilities`（嵌套）；`ClientCapabilities` 既有 `terminal: bool` 又有 `fs: FileSystemCapabilities`。证据：agent.rs L3872-3928 / client.rs L1505-1568。
- codex 实测是**纯简单 flag**：`InitializeCapabilities` 全是 `bool` / `Option<Vec<String>>`，无嵌套（v1.rs L46-57）。
- zhive 选择：**默认简单 flag**（6 个：hooks / subagents / cancellation / permission / extension / experimental_api），**仅 `streaming` 嵌套**（因 Pi 三队列模型 `steer/follow_up/next_turn` 三子能力天然分组）。
- 理由：
  1. 简单 flag 写读快、JSON 体积小、错误处理少；
  2. 未来子能力分级时通过 `#[non_exhaustive]` + `_meta` 渐进升级，**无需破坏现有 bool 字段**；
  3. 嵌套对象 `{ hooks: { version: "1" } }` 形态的扩展性优势在 zhive 不重要——zhive 用 `protocolVersion` 整型 + capability flag 二维矩阵已足够表达版本×能力。

### Q3：v1/v2 method 命名空间编码规则——`v1/initialize` / `zhive.v1/initialize` / 其他？

**答**：**裸 `initialize`，无 namespace 前缀**。

- codex 实测：method = `"initialize"`、`"thread/start"`、`"thread/resume"` —— **从不出现 `v1/` 或 `v2/` 前缀**。证据：common.rs L2010 / L445。
- ACP 实测：`INITIALIZE_METHOD_NAME = "initialize"`，所有方法常量见 `AGENT_METHOD_NAMES` 表，均无前缀。证据：agent.rs L4653 / L4604。
- zhive 选择：**对齐**——method 字符串走 `"<group>/<verb>"`（如 `thread/start` / `permission/request`），握手方法保持裸 `initialize` / `initialized`。
- v1/v2 区分**完全交由 `protocolVersion: u16` + `capabilities.experimentalApi: bool` 两个字段承担**，wire 上不出现版本前缀。
- 不选 `zhive.v1/initialize` 形态，理由：与 codex/ACP/LSP 生态摩擦；前缀不能解决「同一方法签名跨版本不兼容」的真实问题（仍需 capability gate）。

---

## 7. 未决项汇总（TODO）

1. **TODO(开放项-A2.1)**：codex `protocol/v2/` 没有 `Initialize` 类型；本 deliverable 对「双命名空间共存」的解读为「源码模块层 v1/v2 + wire 层无前缀 + capability 协商」三件套。该解读需在 D-007 决策文档里固化。
2. **TODO(开放项-A2.2)**：ACP 端版本协商失败的精确错误码常量未在 `agent-client-protocol-schema-0.12.0` sub-crate 内定位；建议 B1 阶段查 `agent-client-protocol = 0.12.1` 主 crate `error.rs` 确认。
3. **TODO(开放项-A2.3)**：是否纳入 `initialized` notification（codex 有，ACP 无）——本文档建议**纳入**，但 D-007 文本未声明，需 PR 走流程。
4. **TODO(开放项-A2.4)**：`Capabilities.cancellation` 默认 `true` 与其他 flag 默认 `false` 不对称，会让 `Capabilities::default()` 含非零字段，需 A3 review serde `#[serde(default = "default_true")]` 是否引发歧义。
5. **TODO(开放项-A2.5)**：ACP `SessionCapabilities` (agent.rs L3883) 内子字段未深挖，A3 (permission/streaming) 若涉及需补对齐表。
6. **TODO(开放项-A2.6)**：plan §4 A2「v1/v2 命名空间共存」表述与本 deliverable 重定义一致性需确认；如 D-007 原意是 wire 层 `v1/` 前缀，则本 deliverable §5 §6.Q3 结论需重做。
7. **TODO(开放项-A2.7)**：`opt_out_notification_methods` 是放在 capabilities 内（codex 做法）还是放在独立 `subscriptions` 字段，未在 D-007 固化；本文档暂随 codex。
