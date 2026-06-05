# Fork / branch + branch_summary span（范围权衡）

## harnessRef
两种语义模型，必须分清：codex `core/src/thread_manager.rs:621-647` `spawn_subagent_from_forked_history` / `start_thread_with_options_and_fork_source(options, Some(forked_from_thread_id))` = **新 thread 模型**：读源 thread 持久化历史(`stored_thread_to_initial_history` rollout.rs:641)→开一个全新 thread 带 initial history→记录 forked_from（zhive 的 `Thread.forked_from` + subagent_spawn.rs 的"新 child thread + parent 关联"已是这个形态的近亲）。pi `packages/agent/src/harness/agent-harness.ts:737-833` `navigateTree(targetId)` = **同 thread leaf 指针移动模型**：`phase="branch_summary"`→`collectEntriesForBranchSummary(oldLeafId, targetId)`→可选 `generateBranchSummary` LLM 调用→`session.moveTo(newLeafId, summary)` append 一个 `branch_summary` entry 并切 leaf（agent-harness.ts:477 `setLeafId(write.targetId)`），原分支 entry 全保留。zhive 落地的是 codex 的**新 thread 模型**：读源 thread 的 JSONL rollout（真理源，含内存窗口外的历史）→分配全新 thread id→把源 items 重放进新 thread→记录 `Thread.forked_from` + `parent_session` rollout 头；pi 的同 thread leaf-pointer 仅作为研究中考虑过的替代形态，`RolloutEntry::Leaf{target_id}` 用于标记分支头的崩溃恢复点而非作为活跃叶指针。B9-tracing.md §2.2 把 `zhive.branch_summary` 列为 BranchSummary phase 容器 span；decision-diffs.md:280-292 §1.14 已采纳该 span，:188 已采纳 reserved `PreBranchSummary/PostBranchSummary` hook。

## approach
按 codex `fork_thread` 的"跨 thread 完整 fork（rollout 重放 + 开新 thread）"模型落地，由 `engine/fork.rs` 实现。fork 直接读源 thread 的 JSONL rollout（真理源，含内存 items_tail 窗口外的历史），不依赖 B2 lazy-load。具体范围：(1) `RolloutWriter::set_leaf_id(target_id: Option<&str>)` 写 `Leaf{target_id: Some(...)}` 行 + `StorageWriteOp::SetLeaf{thread_id,target_id}`，标记 fork 出的新 thread 的分支头（崩溃恢复点）；(2) `Submission::Fork{source_thread_id, up_to_item: Option<ItemId>, summarize: bool}` + `ForkReply`/`ForkError` + dispatch 分支 + `Engine::fork_thread(...)` 公开方法；(3) `EngineInner::fork_thread`：`Idle→BranchSummary` CAS（复用 compaction.rs 的 try_set_phase_atomic 模式）→广播 PhaseChanged→分配全新 thread id→等源 rollout flush ack（超时回退读当前磁盘态）→读源 rollout 重放为 items（`up_to_item` 截断，inclusive；`None` 重放全量）→（summarize=true 时）走 compaction 同款 `summarize()` 复用生成 branch-summary item 前置→按严格顺序写新 thread rollout：`ForkHeader{parent_session}` → `ThreadUpserted{forked_from: Some(source)}` → `TurnStarted` → N×`ItemAppended` → `TurnEnded{Completed}` → `SetLeaf` → `Flush`→`BranchSummary→Idle`，全程 `.instrument(info_span!("zhive.branch_summary", "session.id"=...))`，phase 回滚由 Drop guard 保证（panic 也能解锁 BranchSummary）。observability.rs 已删 deferred 注释，BRANCH_SUMMARY 纳入 span_literals_match_constants 正式断言。无 storage 的纯内存引擎（`storage = None`）无源 rollout 可读，返回 `ForkError::SourceNotFound`。

## files

- `crates/zhive-core/src/persistence/rollout.rs` — RolloutWriter 的 `pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()>`：append `RolloutEntry::Leaf{target_id: target_id.map(str::to_owned)}` 并 flush（不 fsync，由 caller 决定 save point）。带 doctest。
- `crates/zhive-core/src/persistence/writer.rs` — StorageWriteOp 的 `SetLeaf{thread_id: ThreadId, target_id: Option<String>}` 变体 + apply_op 分派（rollout_for→set_leaf_id→sync_all 作为 save point）；以及 fork 路径所需的 `ForkHeader{thread_id, parent_session}` op（写新 thread rollout 首行的 `Session{parent_session}`）。
- `crates/zhive-core/src/engine/submission.rs` — `Submission::Fork{source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool}`；`ForkReply::Forked{new_thread_id: ThreadId, items_replayed: u32, summarized: bool}` 与 `ForkError{SourceNotFound, EngineBusy{current: EnginePhase}, ReplayFailed{message}, SummarizationFailed{message}}`（手写 Display+Error，与 CompactError 风格一致）；SubmissionReply 的 `Fork(Result<ForkReply, ForkError>)`。
- `crates/zhive-core/src/engine/fork.rs` — fork 模块：`impl EngineInner { pub(in crate::engine) async fn fork_thread(self: &Arc<Self>, source_thread_id, up_to_item, summarize) -> Result<ForkReply, ForkError> }`。流程：storage().ok_or(SourceNotFound)；try_set_phase_atomic(Idle, BranchSummary)→EngineBusy；广播 PhaseChanged{Idle→BranchSummary}；分配新 thread id；Drop guard 武装 phase 回滚（panic 也解锁）；等源 rollout flush ack（5s 超时回退读磁盘）→读源 JSONL 重放为 items（`up_to_item` inclusive 截断）；summarize 时复用 compaction summarize 生成 `[branch summary]` 前缀 item，全程 `info_span!("zhive.branch_summary", "session.id"=...)`；按 `ForkHeader{parent_session}` → `ThreadUpserted{forked_from: Some(source)}` → `TurnStarted` → N×`ItemAppended` → `TurnEnded{Completed}` → `SetLeaf` → `Flush` 顺序写新 thread rollout；BranchSummary→Idle。
- `crates/zhive-core/src/engine/inner.rs` — dispatch 加 `Submission::Fork{..}` 分支调 self.fork_thread(...) 并回 SubmissionReply::Fork。
- `crates/zhive-core/src/engine.rs` — mod 列表加 `mod fork;`；公开 `pub async fn fork_thread(&self, source_thread_id, up_to_item, summarize) -> Result<Result<ForkReply, ForkError>, EngineError>`（照 compact() 的双层 Result + submit_with_reply 模式）+ doctest。
- `crates/zhive-core/src/observability.rs` — span_literals_match_constants 把 `spans::BRANCH_SUMMARY` 纳入正式断言（deferred 注释已删）；span_emission_tests 含 `fork_opens_zhive_branch_summary_span`（seed 一个 turn→engine.fork_thread(...,summarize=true)→断言 recorded 含 "zhive.branch_summary"）。
- `crates/zhive-proto/src/hook.rs` — HookEvent 的 reserved `PreBranchSummary(PreBranchSummaryInput)`/`PostBranchSummary(PostBranchSummaryInput)` 两 case（payload flatten HookEventBase + entries_count），与 decision-diffs §1.7/§1.14 对齐，含 serde round-trip 测试；dispatch 实际接线（cancel 语义）推后到 Phase 2。

## newTypes

- Submission::Fork { source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool }
- enum ForkReply { Forked { new_thread_id: ThreadId, items_replayed: u32, summarized: bool } }
- enum ForkError { SourceNotFound, EngineBusy { current: EnginePhase }, ReplayFailed { message: String }, SummarizationFailed { message: String } } // + impl fmt::Display + std::error::Error
- SubmissionReply::Fork(Result<ForkReply, ForkError>)
- impl RolloutWriter { pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()> }
- StorageWriteOp::SetLeaf { thread_id: ThreadId, target_id: Option<String> }
- StorageWriteOp::ForkHeader { thread_id: ThreadId, parent_session: ThreadId }
- impl EngineInner { pub(in crate::engine) async fn fork_thread(self: &Arc<Self>, source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool) -> Result<ForkReply, ForkError> }
- impl Engine { pub async fn fork_thread(&self, source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool) -> Result<Result<ForkReply, ForkError>, EngineError> }
- HookEvent::PreBranchSummary / PostBranchSummary + 对应 Input struct (reserved)

## redlineImpact
不触红线。无新 crate 依赖：复用既有 tokio/serde/thiserror/tracing/futures/llmsdk；summarize 直接复用 compaction 的 `summarize(provider, items)`（同 provider trait，不平行造轮子）。无 unsafe。无新 unwrap/expect 在非测试码：所有错误走 `?` + 手写 Display+Error 枚举（ForkError 与 CompactError 一致而非 derive thiserror，保持本模块既有风格）。公开 API（Engine::fork_thread、RolloutWriter::set_leaf_id）带 doc comment + doctest（照 compact()/set 现有 doctest）。Submission/HookEvent 均为 `#[non_exhaustive]`，加变体不破坏 wire 决策（D-012 "至少 14"+ decision-diffs §1.10 同理）。单文件控制：fork.rs 独立模块避免 inner.rs 超 600 行。

## crossModuleDeps

- 与 B9 tracing：本方案落地 `zhive.branch_summary` 的真插桩（fork.rs 是该 phase / span 的首个真实 producer），消除 observability 的 deferred 注释——这闭合了 B9 缺口『branch_summary span 无插桩』，observability 测试与真插桩同 PR 改齐，span_literals_match_constants 与真插桩同步。
- 与 B3 persistence：依赖 `RolloutEntry::Leaf.target_id` + writer SetLeaf op + ForkHeader op（写新 thread rollout 首行 `Session{parent_session}`）。Leaf 语义：target_id=None 是 turn save point（turn 完成标记），target_id=Some 是 fork 出的新 thread 的分支头（崩溃恢复点）；rebuild 从源 rollout 的 ForkHeader 与 ThreadUpserted 重建 `forked_from`，新 thread 可独立 rebuild。
- 与 #8『client-native + fork』任务：本方案即跨 thread fork（新 thread + `forked_from` + `parent_session` 头），fork 直接读源 JSONL rollout，不依赖 B2 lazy-load。Phase 2 计划：把 subagent spawn 统一到本 fork 路径——forked subagent 即用父 thread（部分）历史 seed 的 child thread，正是本模块产出（今日 subagent_spawn 用 `ThreadHandle::new_child` 以空 transcript 起 child）。
- 与 A4 hook：PreBranchSummary/PostBranchSummary reserved 类型已加，须与 HookHost::dispatch 的 14+ 事件注册表对齐（decision-diffs §1.7）；实际 dispatch 接线时 hook 失败按 compaction 的 dispatch_compact_hook『log-and-proceed 内部维护』语义，不能让 hook 否决 fork。

## tests

- 单测：RolloutWriter::set_leaf_id 写 Leaf{target_id:Some} 后 read_all 能 round-trip 出该 target_id（rollout.rs tests，照 append_and_read_round_trip）
- 单测：fork_thread 在 Idle 下 summarize=false → Forked{new_thread_id, items_replayed, summarized:false}，源 JSONL 重放进新 thread，phase 回 Idle（照 compaction run_compaction_replaces_history... 模式）
- 单测：fork_thread 在非 Idle（先 CAS 到 Turn）→ EngineBusy（照 run_compaction_busy_when_not_idle）
- 单测：无 storage 的纯内存引擎 fork → SourceNotFound
- 单测：fork 中 panic 时 Drop guard 把 phase 从 BranchSummary 回滚到 Idle（branch_summary_guard_rolls_back_on_panic）
- 单测：rebuild 从 fork 出的新 thread rollout 恢复 forked_from（含 ForkHeader parent_session 头）
- span 集成测试：fork_opens_zhive_branch_summary_span（observability.rs span_emission_tests，SpanCapture 断言含 zhive.branch_summary）
- writer e2e：enqueue SetLeaf op 后 JSONL 末行是 Leaf{target_id:Some}（照 writer_applies_ops_and_persists_items）
- doctest：Engine::fork_thread（no_run，spawn→对未知 thread 返回 SourceNotFound）；RolloutWriter::set_leaf_id（tempfile round-trip）
- doctest：PreBranchSummary serde round-trip（照 hook.rs HookEvent 现有 doctest）

## risks
中低。主要风险：(1) fork 读源 thread 的 JSONL rollout（真理源，含内存 items_tail 窗口外的全量历史），不受 256 窗口限制，也不依赖 B2 lazy-load；`up_to_item` 是对重放历史的 inclusive 截断点。读 rollout 失败（I/O 或损坏行）走 ReplayFailed。(2) Leaf{target_id=None}(turn 标记) 与 Leaf{target_id=Some}(分支头) 共用同一 enum，rebuild 必须不把分支头 Leaf 误当 turn 完成——已在 crossModuleDeps 约定语义。(3) fork 与 compaction 都要求 Idle，二者互斥靠 BranchSummary/Compaction phase CAS 天然保证，无并发风险。(4) summarize=true 复用 compaction summarize 会真打 provider，测试需用 ScriptedModel 避免真实网络（项目已有模式）。回滚由 Drop guard 保证：fork 内 panic 也能把 phase 从 BranchSummary 解锁回 Idle，不会 wedge 引擎。

## recommendation
**落地形态：codex 跨 thread fork 模型（新 thread + rollout 重放 + forked_from + parent_session 头），保证 Phase 1 核心完整。** 理由：(a) Phase 1 核心完整的判定标准是『EnginePhase 5 态全部有真实触发路径 + 每态对应 span 真插桩 + Leaf 指针不是死字段』——fork 是 BranchSummary phase / `zhive.branch_summary` span 的首个真实 producer，并真写 SetLeaf。(b) codex 的新 thread 模型直接读源 JSONL rollout 作真理源，不触 B2 lazy-load（fork 自带从 rollout 重放历史的路径），与既有 subagent_spawn"新 child thread + parent 关联"形态同源、无返工。(c) decision-diffs §1.14 已采纳 zhive.branch_summary 容器 span，fork 是其唯一落地路径。**实现顺序**：1) rollout.rs set_leaf_id + 单测（最底层、零依赖）；2) writer.rs SetLeaf / ForkHeader op；3) submission.rs 类型 + fork.rs 核心逻辑（复用 compaction summarize 骨架）；4) inner.rs dispatch + engine.rs 公开方法 + doctest；5) observability.rs span 集成测试（闭合 B9 缺口）；6) hook.rs Pre/PostBranchSummary reserved 类型（类型+serde 测试，对齐 decision-diffs §1.7 避免后续 wire break）。**推后到 Phase 2 的**：把 subagent spawn 统一到 fork 路径（forked subagent = 用父历史 seed 的 child thread，今日 subagent_spawn 用 `ThreadHandle::new_child` 起空 transcript child）、PreBranchSummary/PostBranchSummary hook 的实际 dispatch 接线与 cancel 语义。fork.rs 模块级 `// TODO(phase2)` 注释记录 subagent 统一项，与 compaction.rs durability 推后注释同风格。
