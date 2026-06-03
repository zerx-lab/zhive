# Fork / branch + branch_summary span（范围权衡）

## currentState
已就位但未接线：(1) `EnginePhase::BranchSummary` 定义于 crates/zhive-proto/src/hook.rs:113；转换表 crates/zhive-core/src/engine/phase.rs:33,35 已允许 `Idle→BranchSummary` 与 `BranchSummary→Idle`，但全工程无任何代码调用 `try_set_phase_atomic(Idle, BranchSummary)`——`Submission` 枚举 (crates/zhive-core/src/engine/submission.rs:143-199) 无 BranchSummary/Fork 变体，dispatch (crates/zhive-core/src/engine/inner.rs:350-418) 无对应分支。(2) `RolloutEntry::Leaf { target_id: Option<String> }` 定义 crates/zhive-core/src/persistence/rollout.rs:54-59；唯一写入点 writer.rs:384 永远写 `target_id: None`（turn 结束标记），无 `set_leaf_id`/fork 写入路径；read_all (rollout.rs:145-164) 会读回 Leaf 但 rebuild (writer.rs:558-560) 对 Leaf 走 `_ => {}` 忽略。(3) `Thread.forked_from: Option<ThreadId>` (crates/zhive-proto/src/domain.rs:132) 已全链路接通 state_db（写 state_db.rs:121,144；读 451-469；建表索引 idx_threads_forked 见 B3 §3.1），但所有构造点 (lifecycle.rs:155/320/416、subagent_spawn.rs:296、writer.rs:520) 都硬编码 `forked_from: None`。(4) `spans::BRANCH_SUMMARY = "zhive.branch_summary"` (observability.rs:44) 仅有常量，无任何 `info_span!("zhive.branch_summary")` 插桩，span_literals_match_constants 测试 (observability.rs:144-149) 明确注释 "deferred with fork feature"。(5) 无 `PreBranchSummary`/`PostBranchSummary` HookEvent 变体（hook.rs 仅有 PreCompact/PostCompact，BranchSummary 只作为 EnginePhase 出现）。

## harnessRef
两种语义模型，必须分清：codex `core/src/thread_manager.rs:621-647` `spawn_subagent_from_forked_history` / `start_thread_with_options_and_fork_source(options, Some(forked_from_thread_id))` = **新 thread 模型**：读源 thread 持久化历史(`stored_thread_to_initial_history` rollout.rs:641)→开一个全新 thread 带 initial history→记录 forked_from（zhive 的 `Thread.forked_from` + subagent_spawn.rs 的"新 child thread + parent 关联"已是这个形态的近亲）。pi `packages/agent/src/harness/agent-harness.ts:737-833` `navigateTree(targetId)` = **同 thread leaf 指针移动模型**：`phase="branch_summary"`→`collectEntriesForBranchSummary(oldLeafId, targetId)`→可选 `generateBranchSummary` LLM 调用→`session.moveTo(newLeafId, summary)` append 一个 `branch_summary` entry 并切 leaf（agent-harness.ts:477 `setLeafId(write.targetId)`），原分支 entry 全保留。zhive 的 `RolloutEntry::Leaf{target_id}` + B3 §4.4 "fork=显式写 leaf 行切指针，原 leaf 保留" 正是 pi 模型。B9-tracing.md §2.2 把 `zhive.branch_summary` 列为 BranchSummary phase 容器 span；decision-diffs.md:280-292 §1.14 已采纳该 span，:188 已采纳 reserved `PreBranchSummary/PostBranchSummary` hook。

## approach
选 **方案 C（中间档）**：补「最小但语义完整的 leaf-pointer 分支」+ span 插桩 + 占位的 summary 钩子，按 pi navigateTree 的"同 thread leaf 移动"模型落地，**不做**方案 A 的"跨 thread 完整 fork（rollout 重放+开新 thread）"。具体范围：(1) `RolloutWriter::set_leaf_id(target_id: Option<&str>)` 真写 `Leaf{target_id: Some(...)}` 行（rollout.rs 已有 enum，只缺写方法）+ `StorageWriteOp::SetLeaf{thread_id,target_id}`；(2) 新增 `Submission::BranchSummary{thread_id, target_item_id, summarize: bool}` + `BranchSummaryReply`/`BranchSummaryError` + dispatch 分支 + `Engine::branch_summary(...)` 公开方法；(3) `EngineInner::run_branch_summary`：`Idle→BranchSummary` CAS（复用 compaction.rs:107 的 try_set_phase_atomic 模式）→广播 PhaseChanged→（summarize=true 时）走 compaction.rs:263 同款 `summarize()` 复用→截断 in-memory items_tail 到 target+summary item→`set_leaf_id` 写 JSONL→`BranchSummary→Idle`，全程 `.instrument(info_span!("zhive.branch_summary", "session.id"=...))`，删 observability.rs:144-149 的 deferred 注释并把 BRANCH_SUMMARY 加入 span_literals_match_constants 断言；(4) rebuild (writer.rs) 处理带 target_id 的 Leaf：把 leaf 之后/分支外 entry 标记为非活跃（最小实现：rebuild 时记录最后一个 Leaf.target_id 作为活跃叶，DB 不变只加日志）。**否决方案 A** 理由：跨 thread fork 需要 (a) rollout 完整重放成 items（zhive 当前 rebuild 只重建 SQL 索引不重建 items_tail，items 仅在内存，要新写"从 JSONL 加载历史进 ThreadStore"的 lazy-load 路径——B2 明确推后），(b) 新 thread 分配+forked_from 写入+SessionHeader.parent_session 写入，工作量是 C 的 3 倍且踩 B2 lazy-load 未实现的硬边界，Phase 1 核心完整性不需要"跨 thread fork"。**否决方案 B（纯占位）** 理由：只补 span 不补 leaf 写入，会让 BranchSummary phase 永远无人触发、`Leaf.target_id` 永远是死字段，span 测试只能断言常量不能断言真插桩，等于把缺口留给 Phase 2 且留下两处"看似实现实则空转"的腐化点。

## files

- `crates/zhive-core/src/persistence/rollout.rs` — 在 RolloutWriter 加 `pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()>`：append `RolloutEntry::Leaf{target_id: target_id.map(str::to_owned)}` 并 flush（不 fsync，由 caller 决定 save point）。补 doctest。
- `crates/zhive-core/src/persistence/writer.rs` — StorageWriteOp 加 `SetLeaf{thread_id: ThreadId, target_id: Option<String>}` 变体 + apply_op 分派 + `apply_set_leaf`（rollout_for→set_leaf_id→sync_all 作为 save point）。rebuild_state_from_rollout 的 `_ => {}`(558) 拆出 Leaf 分支：记录 last_leaf_target，循环后用 tracing::debug 记录活跃叶（DB schema 不动，仅诊断）。
- `crates/zhive-core/src/engine/submission.rs` — 新增 `Submission::BranchSummary{thread_id: ThreadId, target_item_id: ItemId, summarize: bool}`；新增 `BranchSummaryReply{Summarized{leaf_moved_to: ItemId}, Moved}` 与 `BranchSummaryError{ThreadNotFound, EngineBusy{current: EnginePhase}, TargetNotFound, SummarizationFailed{message}}`（impl Display+Error，照 CompactError:116-133）；SubmissionReply 加 `BranchSummary(Result<BranchSummaryReply, BranchSummaryError>)`。
- `crates/zhive-core/src/engine/branch_summary.rs` — 新建模块（参照 compaction.rs 结构，~200 行）：`impl EngineInner { pub(in crate::engine) async fn branch_summary(self: &Arc<Self>, thread_id, target_item_id, summarize) -> Result<BranchSummaryReply, BranchSummaryError> }`。流程：threads().get→ThreadNotFound；try_set_phase_atomic(Idle, BranchSummary)→EngineBusy；广播 PhaseChanged{Idle→BranchSummary}；在 items_tail 定位 target_item_id→TargetNotFound；summarize 时复用 compaction::summarize(provider, &before_target) 包 `info_span!("zhive.branch_summary", "session.id"=%tid.0)`；截断 items_tail 到 [..=target]（+可选 summary AgentMessage）；enqueue_storage_op(SetLeaf{thread_id, target_id:Some(target_item_id)})；leave_branch_summary（BranchSummary→Idle + PhaseChanged）。错误路径回滚 phase（照 compaction.rs:133）。
- `crates/zhive-core/src/engine/inner.rs` — dispatch (350) 加 `Submission::BranchSummary{..}` 分支调 self.branch_summary(...) 并回 SubmissionReply::BranchSummary。
- `crates/zhive-core/src/engine.rs` — mod 列表(33-42)加 `mod branch_summary;`；加公开 `pub async fn branch_summary(&self, thread_id, target_item_id, summarize) -> Result<Result<BranchSummaryReply, BranchSummaryError>, EngineError>`（照 compact() 615-657 的双层 Result + submit_with_reply 模式）+ doctest。
- `crates/zhive-core/src/observability.rs` — 删除 span_literals_match_constants(144-149) 的 deferred 注释块，把 `spans::BRANCH_SUMMARY` 纳入正式断言；在 span_emission_tests 加 `branch_summary_opens_zhive_branch_summary_span` 测试（seed 一个 turn→engine.branch_summary(...,summarize=true)→断言 recorded 含 "zhive.branch_summary"）。doc 注释(25)保留。
- `crates/zhive-proto/src/hook.rs` — （可选，本阶段建议做）在 HookEvent 加 reserved `PreBranchSummary(PreBranchSummaryInput)`/`PostBranchSummary(PostBranchSummaryInput)` 两 case（payload flatten HookEventBase + entries_count），与 decision-diffs §1.7/§1.14 对齐；若本阶段不接 dispatch 则仅加类型+serde round-trip doctest。

## newTypes

- Submission::BranchSummary { thread_id: ThreadId, target_item_id: ItemId, summarize: bool }
- enum BranchSummaryReply { Summarized { leaf_moved_to: ItemId }, Moved }
- enum BranchSummaryError { ThreadNotFound, EngineBusy { current: EnginePhase }, TargetNotFound, SummarizationFailed { message: String } } // + impl fmt::Display + std::error::Error
- SubmissionReply::BranchSummary(Result<BranchSummaryReply, BranchSummaryError>)
- impl RolloutWriter { pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()> }
- StorageWriteOp::SetLeaf { thread_id: ThreadId, target_id: Option<String> }
- impl EngineInner { pub(in crate::engine) async fn branch_summary(self: &Arc<Self>, thread_id: ThreadId, target_item_id: ItemId, summarize: bool) -> Result<BranchSummaryReply, BranchSummaryError> }
- impl Engine { pub async fn branch_summary(&self, thread_id: ThreadId, target_item_id: ItemId, summarize: bool) -> Result<Result<BranchSummaryReply, BranchSummaryError>, EngineError> }
- (可选) HookEvent::PreBranchSummary / PostBranchSummary + 对应 Input struct

## redlineImpact
不触红线。无新 crate 依赖：复用既有 tokio/serde/thiserror/tracing/futures/llmsdk；summarize 直接复用 compaction.rs:263 的 `summarize(provider, items)`（同 provider trait，不平行造轮子）。无 unsafe。无新 unwrap/expect 在非测试码：所有错误走 `?` + 新 thiserror 风格枚举（BranchSummaryError 手写 Display+Error，与 CompactError 一致而非 derive thiserror，保持本模块既有风格）。公开 API（Engine::branch_summary、RolloutWriter::set_leaf_id）必须带 doc comment + doctest（照 compact()/set 现有 doctest）。Submission/HookEvent 均为 `#[non_exhaustive]`，加变体不破坏 wire 决策（D-012 "至少 14"+ decision-diffs §1.10 同理）。单文件控制：branch_summary.rs 新建独立模块避免 inner.rs 超 600 行。

## crossModuleDeps

- 与 B9 tracing：本方案落地 `zhive.branch_summary` 的真插桩，消除 observability.rs:144-149 的 deferred 注释——这是 B9 缺口『branch_summary span 无插桩』的直接闭合点，两者必须同一 PR 改 observability 测试，否则 span_literals_match_constants 与真插桩不同步。
- 与 B3 persistence：依赖 `RolloutEntry::Leaf.target_id`（已存在）+ writer SetLeaf 新 op；rebuild 对带 target_id Leaf 的处理须与 writer.rs 现有『Leaf=turn 完成标记(target_id=None)』语义并存——约定：target_id=None 仍是 turn save point，target_id=Some 是 fork/branch 切叶，rebuild 两者都不重建 item 表只记活跃叶。
- 与 #8『client-native + fork』任务：本方案是 leaf-pointer 同 thread 模型；若 client-native 或后续要『跨 thread fork（新 thread+forked_from+SessionHeader.parent_session）』，需等 B2 lazy-load-from-jsonl 落地后另起方案，本阶段 forked_from 写入路径保持硬编码 None 不动，避免半实现。
- 与 A4 hook：PreBranchSummary/PostBranchSummary 若加，须与 HookHost::dispatch 的 14+ 事件注册表对齐（decision-diffs §1.7 已 reserved），且 hook 失败按 compaction.rs:dispatch_compact_hook 的『log-and-proceed 内部维护』语义，不能让 hook 否决 branch summary。

## tests

- 单测：RolloutWriter::set_leaf_id 写 Leaf{target_id:Some} 后 read_all 能 round-trip 出该 target_id（rollout.rs tests，照 append_and_read_round_trip）
- 单测：run_branch_summary 在 Idle 下 summarize=false → Moved，items_tail 截断到 target，phase 回 Idle（照 compaction.rs:run_compaction_replaces_history... 模式）
- 单测：run_branch_summary 在非 Idle（先 CAS 到 Turn）→ EngineBusy（照 run_compaction_busy_when_not_idle:411）
- 单测：target_item_id 不在 items_tail → TargetNotFound
- span 集成测试：branch_summary_opens_zhive_branch_summary_span（observability.rs span_emission_tests，SpanCapture 断言含 zhive.branch_summary）
- writer e2e：enqueue SetLeaf op 后 JSONL 末行是 Leaf{target_id:Some}（照 writer_applies_ops_and_persists_items:691）
- doctest：Engine::branch_summary（no_run，spawn→对未知 thread 返回 ThreadNotFound）；RolloutWriter::set_leaf_id（tempfile round-trip）
- （若做 hook）doctest：PreBranchSummary serde round-trip（照 hook.rs HookEvent 现有 doctest:163-174）

## risks
中低。主要风险：(1) items_tail 是有界窗口(256, thread.rs:83)，target_item_id 可能已被驱逐出内存——本阶段约定 target 必须在内存窗口内，否则 TargetNotFound（与 compaction 同样只操作 items_tail 的局限一致；完整从 JSONL 找历史 target 须等 B2 lazy-load，不在本范围）。(2) Leaf{target_id=None}(turn 标记) 与 Leaf{target_id=Some}(切叶) 共用同一 enum，rebuild 必须不把切叶 Leaf 误当 turn 完成——已在 crossModuleDeps 约定语义。(3) branch_summary 与 compaction 都要求 Idle 且都截断 items_tail，二者互斥靠 phase CAS 天然保证，无并发风险。(4) summarize=true 复用 compaction summarize 会真打 provider，测试需用 ScriptedModel 避免真实网络（项目已有模式）。无回滚风险：phase 错误路径已照 compaction.rs:133 回滚。

## recommendation
**建议范围档：C（中间档），保证 Phase 1 核心完整但不过度。** 理由：(a) Phase 1 核心完整的判定标准是『EnginePhase 6 态全部有真实触发路径 + 每态对应 span 真插桩 + Leaf 指针不是死字段』——方案 B 留两个空转点不达标，方案 A 的跨 thread fork 触碰 B2 lazy-load 未实现硬边界属于过度。(b) zhive 已选 pi 的 leaf-pointer 模型（B3 §4.4 + RolloutEntry::Leaf.target_id 设计），方案 C 正是把这个已设计但未接线的能力补全，与既有架构一致、无返工。(c) decision-diffs §1.14 已采纳 zhive.branch_summary 容器 span，本方案是其唯一落地路径。**实现顺序**：1) rollout.rs set_leaf_id + 单测（最底层、零依赖）；2) writer.rs SetLeaf op + rebuild Leaf 分支；3) submission.rs 类型 + branch_summary.rs 核心逻辑（复用 compaction 骨架，可大量 copy-adapt）；4) inner.rs dispatch + engine.rs 公开方法 + doctest；5) observability.rs 删 deferred 注释 + span 集成测试（闭合 B9 缺口）；6) **可选但建议**：hook.rs 加 Pre/PostBranchSummary reserved 类型（仅类型+serde 测试，dispatch 接线可推后，对齐 decision-diffs §1.7 避免后续 wire break）。**明确推后到 Phase 2 的**：跨 thread fork（新 thread+rollout 重放+forked_from 写入+SessionHeader.parent_session）、从 JSONL 历史定位窗口外 target（依赖 B2 lazy-load）、PreBranchSummary hook 的实际 cancel 语义。把这三项写进 branch_summary.rs 模块级 `// TODO(phase2-cross-thread-fork)` 注释，与 compaction.rs:9-19 的 durability 推后注释同风格。
