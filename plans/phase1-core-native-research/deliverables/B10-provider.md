---
title: B10 · LLM provider 抽象（Phase 1 占位）
status: draft
date: 2026-05-28
depends_on:
  - plan §5 B10（约 line 517-535）
  - plan §9 R-4（line 698）
  - deliverables/A1-thread-turn-item.md（zhive `Item` 14 case）
  - deliverables/B1-engine-loop.md（actor pattern + mpsc 拓扑）
inputs:
  - llmsdk crate（git 依赖：`https://github.com/zerx-lab/llmsdk@main`，本地：`/home/zero/Desktop/code/zerx-lab/llmsdk/`）
  - ⚠️ 反例：`~/Desktop/code/github/cline/providers/*.ts`（未本地 clone，跳过审，仅取「不要每个 provider 一个 PR」结论）
outputs:
  - zhive crate `provider` 抽象选型
---

## 0. TL;DR

**决策：直接复用 `llmsdk::language_model::LanguageModel` trait 作为 zhive provider boundary**，无需 zhive 自己造 `LlmProvider` trait。zhive core 持有 `DynLanguageModel`（即 `Arc<dyn LanguageModel>`，cheap clone），engine loop 调 `do_stream(CallOptions) -> Result<StreamResult>` 拿 `BoxStream<Result<StreamPart>>`，在 engine actor 内做 `StreamPart -> zhive Item` 的折叠转换。

**R-4 未触发**：llmsdk trait 表面 4 个必需能力（completion / stream / tool_call / reasoning）全部覆盖。需要的「适配」纯发生在 `StreamPart -> Item` 的 fold 逻辑里，不是 trait 设计问题，归到 engine loop（B1）实现。

---

## 1. 参考点清单（带锚点）

| 参考点 | 路径 | 行号 |
|---|---|---|
| `LanguageModel` trait（`do_generate` + `do_stream` + `provider/model_id/specification_version/supported_urls`） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/mod.rs` | 50-97 |
| `Provider` 工厂 trait（`language_model(id) -> DynLanguageModel`，可选 embedding / image） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/provider.rs` | 20-46 |
| `DynLanguageModel = Arc<dyn LanguageModel>` newtype（cheap clone + Deref） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/provider.rs` | 53-83 |
| `StreamPart` enum 20+ case（含 `TextStart/Delta/End`, `ReasoningStart/Delta/End`, `ToolInputStart/Delta/End`, `ToolCall`, `ToolResult`, `ToolApprovalRequest`, `Source`, `File`, `ReasoningFile`, `Custom`, `StreamStart`, `ResponseMetadata`, `Finish`, `Raw`, `Error`） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/stream_part.rs` | 22-204 |
| `Content` enum（非流式：`Text/Reasoning/Custom/ReasoningFile/File/ToolApprovalRequest/Source/ToolCall/ToolResult`） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/content.rs` | 21-58 |
| `ToolCallPart { tool_call_id, tool_name, input: JsonValue, provider_executed, dynamic, provider_options }` | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/prompt.rs` | 181-209 |
| `StreamResult { stream: BoxStream<Result<StreamPart>>, request, ... }` + `BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>` | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/result.rs` | 67-80（StreamResult）/ mod.rs 42（BoxStream） |
| `GenerateResult { content: Vec<Content>, finish_reason, usage, ... }` | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/llmsdk-provider/src/language_model/result.rs` | 19-36 |
| Provider 工厂实现（13 个 `llmsdk-*` 子 crate：`anthropic / openai / google / azure / bedrock / cohere / mistral / xai / google-vertex / anthropic-aws`） | `/home/zero/Desktop/code/zerx-lab/llmsdk/crates/` | dir listing |
| zhive `Item` 14 case（`AgentMessage / Reasoning / ToolCall / ...`） | `deliverables/A1-thread-turn-item.md` | 85-98 |
| engine actor 内 `item_tx: mpsc::Sender<Item>`（折叠转换的下游） | `deliverables/B1-engine-loop.md` | 303 |

---

## 2. 决策：直接复用 `llmsdk::LanguageModel` trait

### 2.1 选项对比

| 方案 | 优点 | 缺点 | 选 |
|---|---|---|---|
| A. **直接复用** `llmsdk::LanguageModel` | trait 表面已覆盖 4 项必需（completion / stream / tool_call / reasoning）；13 个 provider impl 现成；`DynLanguageModel` newtype 已封装 `Arc`，clone 廉价；`StreamPart` 与 zhive `Item` 14 case 几乎 1:1（仅多 2-3 case 是 zhive 已砍的特性） | trait 名带 `do_` 前缀略 unidiomatic；`CallOptions` / `Prompt` 字段较多（含 zhive Phase 1 不用的 `ResponseFormat`、`ToolChoice`） | ✅ |
| B. 包一层 zhive `LlmProvider` adapter | 屏蔽 `do_` 前缀；可只暴露 zhive 必需字段 | 重复 trait（违反 CLAUDE.md「优先复用已有 trait」）；每加一个 provider 都要写一层 adapter；与「不要每个 provider 一个 PR」反例同构 | ✗ |
| C. 自定义 trait | 完全自主 | 13 个 provider impl 全部要重写或反向适配；与「禁止平行造轮子」冲突 | ✗ |

### 2.2 选 A 的硬依据

1. **trait 必需能力覆盖**（关键问题 Q1）：

   | 必需能力 | `llmsdk::LanguageModel` 入口 | 覆盖 |
   |---|---|---|
   | completion（非流式） | `async fn do_generate(&self, CallOptions) -> Result<GenerateResult>`（`mod.rs:79`） | ✅ |
   | stream（流式） | `async fn do_stream(&self, CallOptions) -> Result<StreamResult>`（`mod.rs:96`） | ✅ |
   | tool_call | `StreamPart::ToolCall(ToolCallPart)` + `StreamPart::ToolInputStart/Delta/End`（流式增量）+ `Content::ToolCall(ToolCallPart)`（非流式终态）（`stream_part.rs:101-148`, `content.rs:55`） | ✅ |
   | reasoning | `StreamPart::ReasoningStart/Delta/End` + `Content::Reasoning(ReasoningPart)`（`stream_part.rs:62-99`, `content.rs:28`） | ✅ |

2. **factory 形态契合 zhive engine**：`Provider::language_model(id) -> DynLanguageModel`（`provider.rs:24`）正好让 engine 持有一个 `DynLanguageModel`（cheap clone），不需要 zhive 自己管理 provider 注册表。

3. **流式 chunk 与 zhive Item 14 case 几乎同构**：见 §3 草图，主要折叠路径是 `Reasoning*-{Start,Delta,End} -> Item::Reasoning / AgentThought`、`Text*-{Start,Delta,End} -> Item::AgentMessage`、`ToolCall / ToolInput* -> Item::ToolCall`。

4. **不与 zhive 已有 trait/error 类型冲突**：llmsdk 用自己的 `crate::error::Result`，zhive engine 在 adapter 边界把 `llmsdk::ProviderError` 映到 zhive 自己的 `EngineError`（B1 已留 `EngineError` 占位）。

---

## 3. provider → zhive Item 的转换草图

转换发生在 **engine actor 的 turn loop 内**（B1 deliverable 描述的 `item_tx: mpsc::Sender<Item>` 上游）。不是独立 crate，不是独立 trait，是 engine actor 的一个 `fn fold_stream_part`。

```rust
// 仅 sketch，不在 zhive crate 落地代码。
// 位于 engine actor 内（B1 deliverable 描述）。

use llmsdk::provider::DynLanguageModel;
use llmsdk::language_model::{CallOptions, StreamPart, ToolCallPart, ReasoningPart};
use llmsdk::language_model::content::Content;
use tokio::sync::mpsc;
use futures::StreamExt;

/// engine actor 内的折叠状态。
/// llmsdk `*-Start / *-Delta / *-End` 三段式 → zhive `Item` 单条聚合体。
struct StreamFold {
    /// 当前正在累积的 text block，按 block id 分组。
    text_buf: HashMap<String, String>,
    /// 当前正在累积的 reasoning block，按 block id 分组。
    reasoning_buf: HashMap<String, String>,
    /// 当前正在累积的 tool_call input JSON 片段，按 tool_call_id 分组。
    tool_input_buf: HashMap<String, String>,
}

impl StreamFold {
    /// 把单个 llmsdk `StreamPart` 折叠为 0..N 个 zhive `Item`，推到 item_tx。
    ///
    /// # Errors
    ///
    /// 仅在 mpsc 关闭时返回 EngineError::Cancelled；
    /// llmsdk in-stream error（`StreamPart::Error`）映射为 Item::SystemNotice，不上抛。
    async fn fold(
        &mut self,
        part: StreamPart,
        item_tx: &mpsc::Sender<Item>,
        turn_id: TurnId,
    ) -> Result<(), EngineError> {
        match part {
            // === reasoning chunk → Item::Reasoning / AgentThought ===
            StreamPart::ReasoningStart { id, .. } => {
                self.reasoning_buf.entry(id).or_default();
            }
            StreamPart::ReasoningDelta { id, delta, .. } => {
                self.reasoning_buf.entry(id).or_default().push_str(&delta);
                // 增量也推一条 Item（B1 mpsc 是 item-in-turn 流，每 chunk 一条）
                item_tx.send(Item::AgentThought {
                    id: item_id_from(&id),
                    text: delta,
                }).await.map_err(|_| EngineError::Cancelled)?;
            }
            StreamPart::ReasoningEnd { id, .. } => {
                if let Some(full) = self.reasoning_buf.remove(&id) {
                    // 终态：完整 reasoning 落 Reasoning item（summary 用 provider_metadata 补，或拍平为单 element vec）
                    item_tx.send(Item::Reasoning {
                        id: item_id_from(&id),
                        summary: vec![full],
                    }).await.map_err(|_| EngineError::Cancelled)?;
                }
            }

            // === agent_message（text chunk）→ Item::AgentMessage ===
            StreamPart::TextStart { id, .. } => {
                self.text_buf.entry(id).or_default();
            }
            StreamPart::TextDelta { id, delta, .. } => {
                self.text_buf.entry(id.clone()).or_default().push_str(&delta);
                // 增量推 AgentMessageChunk 语义的 item（A1 已对齐 ACP AgentMessageChunk）
                item_tx.send(Item::AgentMessage {
                    id: item_id_from(&id),
                    text: delta,
                }).await.map_err(|_| EngineError::Cancelled)?;
            }
            StreamPart::TextEnd { id, .. } => {
                self.text_buf.remove(&id); // 终态由调用方在 finish 时聚合，或丢弃
            }

            // === tool_call → Item::ToolCall（status=InProgress→Completed） ===
            StreamPart::ToolInputStart { id, tool_name, dynamic, .. } => {
                self.tool_input_buf.entry(id.clone()).or_default();
                item_tx.send(Item::ToolCall {
                    id: item_id_from(&id),
                    name: tool_name,
                    kind: ToolKind::Other, // 由 zhive tool registry 填正（ACP 10 case：Read/Edit/...）
                    status: ToolCallStatus::Pending,
                    arguments: serde_json::Value::Null,
                    content: vec![],
                    locations: vec![],
                    raw_input: None,
                    raw_output: None,
                }).await.map_err(|_| EngineError::Cancelled)?;
            }
            StreamPart::ToolInputDelta { id, delta, .. } => {
                self.tool_input_buf.entry(id).or_default().push_str(&delta);
                // 增量 JSON 片段不直接推 item（避免 Item::ToolCall 半解析 JSON 抖动），
                // 等 ToolInputEnd 或 ToolCall 终态再推 update
            }
            StreamPart::ToolInputEnd { id, .. } => {
                // 解析累积的 JSON，推一条 ToolCall update（status=InProgress）
                if let Some(raw) = self.tool_input_buf.remove(&id) {
                    let input: serde_json::Value = serde_json::from_str(&raw)
                        .unwrap_or(serde_json::Value::String(raw));
                    item_tx.send(Item::ToolCall {
                        id: item_id_from(&id),
                        status: ToolCallStatus::InProgress,
                        arguments: input,
                        ..Default::default()
                    }).await.map_err(|_| EngineError::Cancelled)?;
                }
            }
            StreamPart::ToolCall(ToolCallPart { tool_call_id, tool_name, input, .. }) => {
                // 终态（非流式 input 或 input stream 已完）
                item_tx.send(Item::ToolCall {
                    id: item_id_from(&tool_call_id),
                    name: tool_name,
                    status: ToolCallStatus::InProgress,
                    arguments: input,
                    ..Default::default()
                }).await.map_err(|_| EngineError::Cancelled)?;
            }

            // === 终止 / 警告 / raw 不上抛为 Item，仅打 tracing ===
            StreamPart::Finish { usage, finish_reason, .. } => {
                tracing::info!(?usage, ?finish_reason, turn_id = ?turn_id, "llmsdk stream finish");
            }
            StreamPart::Error { error } => {
                // 仅 in-stream error，落系统通知（A1 case 14: SystemNotice）
                item_tx.send(Item::SystemNotice {
                    id: ItemId::new(),
                    text: format!("provider error: {error}"),
                }).await.map_err(|_| EngineError::Cancelled)?;
            }
            StreamPart::ToolResult(_) | StreamPart::ToolApprovalRequest(_) => {
                // Phase 1 不处理 provider-executed tool（zhive 自己跑 tool，不依赖 provider 执行）
                tracing::warn!("provider-executed tool ignored in Phase 1");
            }
            _ => { /* StreamStart / ResponseMetadata / Source / File / ReasoningFile / Custom / Raw：Phase 1 不落 Item */ }
        }
        Ok(())
    }
}

/// engine actor turn loop 内的调用点。
async fn run_provider_turn(
    model: DynLanguageModel,           // 来自 EngineInner（Arc 廉价 clone）
    options: CallOptions,
    item_tx: mpsc::Sender<Item>,
    turn_id: TurnId,
) -> Result<(), EngineError> {
    let mut stream_result = model.do_stream(options).await
        .map_err(EngineError::from_provider)?;   // llmsdk::ProviderError → EngineError
    let mut fold = StreamFold::default();
    while let Some(part_res) = stream_result.stream.next().await {
        let part = part_res.map_err(EngineError::from_provider)?;
        fold.fold(part, &item_tx, turn_id).await?;
    }
    Ok(())
}
```

> 注：上述 `Item::ToolCall { ..Default::default() }` 是 sketch，实际 zhive `Item` 14 case 未必都 derive `Default`（A1 未定）；正式实现走 builder 或显式字段。
> 注：`item_id_from(provider_block_id)` 是 zhive 自己的 `ItemId` 生成，不直接复用 provider 的 block id（provider id 命名空间不可信，zhive 用 `ItemId::new()` + 内部映射表关联 provider block id → ItemId）。

---

## 4. 关键问题逐条作答

### Q1：`llmsdk` 已有的 trait 能不能直接当 zhive 的 provider boundary？

**能**。`llmsdk::language_model::LanguageModel`（`mod.rs:50-97`）trait 表面：

- 元数据：`provider()`, `model_id()`, `specification_version()`, `supported_urls()`
- 非流式 completion：`do_generate(CallOptions) -> Result<GenerateResult>`
- 流式：`do_stream(CallOptions) -> Result<StreamResult>`，`StreamResult.stream: BoxStream<Result<StreamPart>>`
- tool_call：通过 `StreamPart::ToolCall(ToolCallPart)` + `StreamPart::ToolInput{Start,Delta,End}` + `Content::ToolCall(ToolCallPart)`
- reasoning：通过 `StreamPart::Reasoning{Start,Delta,End}` + `Content::Reasoning(ReasoningPart)`

4 项全覆盖。`Provider` 工厂 trait（`provider.rs:20-46`）+ `DynLanguageModel` newtype 让 zhive engine 直接持 `Arc<dyn LanguageModel>` 即可。

### Q2：流式响应（reasoning / agent_message chunk）的接口形状

`BoxStream<Result<StreamPart>>`，即 `Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>`（`mod.rs:42` 别名 + `result.rs:75`）。语义双层：

- 外层 `Result::Err`：调用前就失败（认证 / 网络）；
- 内层 `StreamPart::Error`：流活着但 provider 报错（content filter / 部分失败）。

reasoning / agent_message 用三段式 `Start / Delta / End`，按 block `id` 分组（同一 block id 的 delta 是同一段输出的增量）。engine actor 侧在 §3 fold 时按 block id 分桶累积。

### Q3：tool_call schema 在 provider 层 vs zhive Item schema 的转换点（adapter 形态）

转换点在 **engine actor 的 fold 函数内**（§3 的 `StreamFold::fold`），不是独立 crate / trait / adapter 类型。形态：

| llmsdk 端 | zhive `Item::ToolCall` 字段 | 注 |
|---|---|---|
| `ToolCallPart.tool_call_id`（`prompt.rs:184`） | 不直接复用为 `Item.id`；用 zhive `ItemId::new()`，内部映射表存 `provider_id → ItemId` | provider id 命名空间不可信 |
| `ToolCallPart.tool_name` | `Item::ToolCall.name` | 直传 |
| `ToolCallPart.input: JsonValue` | `Item::ToolCall.arguments: serde_json::Value` | 直传（llmsdk `JsonValue` 是 serde_json 别名） |
| `ToolCallPart.provider_executed` | （Phase 1 忽略） | zhive 自己跑 tool |
| `ToolCallPart.dynamic` | （Phase 1 忽略） | zhive tool registry 自有静态/动态区分 |
| `StreamPart::ToolInput{Start,Delta,End}` | `Item::ToolCall.status: Pending → InProgress` | A1 已对齐 ACP `ToolCallStatus` 4 case |
| `StreamPart::ToolResult` / `StreamPart::ToolApprovalRequest` | **Phase 1 忽略** | zhive 不让 provider 执行 tool；自家 tool runtime 跑（B1 dispatch_tool_call） |

**adapter 形态结论**：不需要独立 adapter struct。fold 函数就是 adapter。adapter "薄" 到只能算 engine actor 内的一个 helper，不值得封装为 trait/crate。

---

## 5. R-4 触发判定

**未触发**。llmsdk trait 表面审查后，4 项必需（completion / stream / tool_call / reasoning）全部直接覆盖，且工厂 trait 形态契合 zhive engine 持单 `DynLanguageModel`。差距清单路径不需要走。

唯一接近"差距"的点（详见 §6 未决项）：

- llmsdk `StreamPart::ToolInputEnd` 后没有「完整 JSON 已解析」事件，需 zhive engine 自己解析 `tool_input_buf` 累积的字符串。这是**实现差距**（engine 自己解 JSON），不是**trait 差距**。
- llmsdk `ToolCallPart` 缺 `kind: ToolKind`（ACP 10 case），需 zhive tool registry 在 engine 侧补。也是实现差距。

---

## 6. 未决项

> TODO(开放项 B10-1)：`Item::ToolCall.kind: ToolKind` 的填充时机 —— llmsdk `ToolCallPart` 无 `kind` 字段。方案 (a) tool registry 注册时记录 name→kind 映射，fold 时查表填；(b) 让 zhive tool runtime 启动 tool 时再回填，fold 时 `kind: Other` 占位。建议 (a)，但需 B-tool 子任务确认。

> TODO(开放项 B10-2)：llmsdk `CallOptions` 的 zhive 端 Phase 1 字段集子集 —— `CallOptions` 包含 `ResponseFormat / ToolChoice / ReasoningEffort` 等 Phase 1 可能不用的字段。本任务不展开，留给 B1 engine loop 实现时按需精简（默认全 `None` 即可）。

> TODO(开放项 B10-3)：llmsdk `JsonValue` 是否就是 `serde_json::Value` —— 草图按等价处理。需在实现时校验 `llmsdk::json::JsonValue` 类型别名（`crates/llmsdk-provider/src/json.rs`）。

> TODO(开放项 B10-4)：`StreamPart::Error.error: JsonValue` 的 zhive 端展示 —— 目前 fold 拍平为 `format!("{error}")` 字符串塞到 `Item::SystemNotice`；如要保留 JSON 结构需扩 `Item::SystemNotice` 字段集（已超 A1 14 case scope，建议保留拍平）。

> TODO(开放项 B10-5)：provider 配置 / API key 注入路径 —— llmsdk `Provider` 工厂如何在 zhive 中实例化（环境变量？workspace config？extension manifest？）。本任务只锁定 trait 选型，不锁定配置层。建议归到 A5 extension manifest 或后续 B11+ 占位。

> TODO(开放项 B10-6)：`ItemId` 与 llmsdk `tool_call_id / block id` 的双向映射表生命周期 —— fold 内部需要个 `HashMap<String, ItemId>`，turn 结束时是否 drop？建议 turn 结束 drop（provider id 仅 turn 内有效）。

---

## 7. 不在本任务范围

- llmsdk middleware（`crates/llmsdk-provider/src/middleware/`）：zhive Phase 1 不需要中间件；后续如做 retry / cache / telemetry 再启用。
- llmsdk 其他模态（embedding / image / files / speech / transcription / video / skills / reranking）：Phase 1 仅 language_model。
- llmsdk 各 provider impl crate（13 个 `llmsdk-*`）选型：zhive Phase 1 跑通时挑 1-2 个（建议 `llmsdk-anthropic` + `llmsdk-openai` 起步），不在本调研 scope。
- ✅ **不动 crates**：本 deliverable 仅文档；不在 zhive 任何 crate 引入或落实代码。
- ✅ **不改 99-decisions**：本 deliverable 不修订 D-014 / 现有 decision；若选型与 D-014 偏离，由 plan §10 回流时统一处理。
