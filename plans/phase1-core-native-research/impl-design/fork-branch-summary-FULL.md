# 【全量·档B】跨 thread fork + branch + branch_summary —— 真正的「新 thread + forked_from + SessionHeader.parent_session + JSONL 重放」

## currentState
基线（档C）已设计但未接线；本全量目标在其上叠加 codex 式跨 thread fork。精确现状（文件:行号）：

(1) **EnginePhase::BranchSummary 转换已就位、无触发者**：`crates/zhive-proto/src/hook.rs:113` 定义变体；`crates/zhive-core/src/engine/phase.rs:31-38` 的 `allows_transition` 已含 `(Idle, Turn|Compaction|BranchSummary)` 与 `(Compaction|BranchSummary, Idle)`。全工程无 `try_set_phase_atomic(Idle, BranchSummary)` 调用；`Submission`（submission.rs:143-199）无 Fork/BranchSummary 变体；dispatch（inner.rs:350-418）无对应分支，落到 408 `other => debug!(...unhandled)`。

(2) **Leaf.target_id 永远写 None**：`RolloutEntry::Leaf { target_id: Option<String> }` 定义 rollout.rs:54-59；唯一写入点 writer.rs:384 `RolloutEntry::Leaf { target_id: None }`（turn 结束标记）；无 `set_leaf_id`。read_all（rollout.rs:145-164）能读回 Leaf；rebuild_state_from_rollout 对 Leaf 走 writer.rs:558-560 `_ => {}` 忽略。

(3) **forked_from 全链路接通 state_db、但所有构造点硬编码 None**：`Thread.forked_from: Option<ThreadId>`（domain.rs:132，serde camelCase `forkedFrom`，skip_if None）；state_db 写 121/131/144、读 452/469、列名 `forked_from`（list_threads:343、get_thread:380）。构造点全部 `forked_from: None`：lifecycle.rs:155/320/416、subagent_spawn.rs:299、writer.rs:523。

(4) **SessionHeader.parent_session 字段在但永远 None**：`RolloutEntry::Session { parent_session: Option<String> }` rollout.rs:39-41（serde 默认 + skip_if None）；唯一写入点 writer.rs:283 `parent_session: None`（apply_thread_upserted）；rebuild 读 Session 时丢弃（writer.rs:509-510 `..` 忽略 parent_session）。

(5) **branch_summary span 未插桩**：`spans::BRANCH_SUMMARY = "zhive.branch_summary"`（observability.rs:44）仅常量；`span_literals_match_constants`（observability.rs:144-149）显式注释 "deferred with the fork feature"，仅 `let _ = spans::BRANCH_SUMMARY;`。

(6) **EngineInner 不持有 Arc<Storage>（全量阻塞点）**：EngineConfig.storage（engine.rs:187）在 spawn_with_config（engine.rs:409-419）被消费——仅 `PersistenceWriter::spawn(Arc::clone(s))` 取出 writer tx+handle，原 `Arc<Storage>` 在 416-419 后被 drop。EngineInner（inner.rs:81-148）只有 `storage_writer: Mutex<StorageWriterState>`（写通道），**没有读路径**。跨 thread fork 需读源 thread 的 rollout JSONL（`Storage::rollout_path` + `read_all`），故必须把 `Arc<Storage>` 留存进 EngineInner。

(7) **Compaction 是全量可大量复用的骨架**：compaction.rs:74-191 的 compact/run_compaction（Idle→Compaction CAS、PhaseChanged 广播、`.instrument(info_span!("zhive.compaction"))`、summarize 复用、错误回滚 leave_compaction:177-191）；summarize（compaction.rs:263-311）渲染 Item→文本→provider.do_stream→收集 TextDelta。

(8) **subagent_spawn 是「新 child thread + 注册 + 起 turn + 持久化」的现成范本**：subagent_spawn.rs:96-224 分配 child id、`ThreadHandle::new_child`（thread.rs:136-159，已存 parent_thread_id）、`threads().write_guard().insert`、start_child_turn 入 ThreadUpserted+TurnStarted（subagent_spawn.rs:293-315，但 forked_from:None）。

(9) **B2 lazy-load 仍是占位**：thread.rs:339-341 注释「B2 attaches lazy_load_from_jsonl」；ThreadStore（thread.rs:343-384）纯内存 HashMap，无从 JSONL 载入路径。B2 deliverable 占位 trait `load_items_page(thread_id,turn_id,offset,limit)`（B2:226）按 turn 分页，**不**直接满足 fork 需要的「整源 thread 历史→items」重放。

## harnessRef
**全量真正参考的是 codex 的「新 thread 模型」（不是 pi 的同 thread leaf 移动）。** 两份 harness 的精确映射：

**A. codex 跨 thread fork（全量主参考，~/Desktop/code/github/codex/codex-rs/）**：
- `core/src/thread_manager.rs:817-840` `fork_thread<S>(snapshot, config, path, ...)`：入口，`initial_history_from_rollout_path(path)` 读源 rollout → `fork_thread_from_history`。
- `core/src/thread_manager.rs:842-858` `initial_history_from_rollout_path`：`thread_store.read_thread_by_rollout_path(include_history:true)` → `stored_thread_to_initial_history`。**这就是 zhive 缺的「从 JSONL 重放成 items」**。
- `core/src/thread_manager.rs:1370-1385` `stored_thread_to_initial_history`：`StoredThread.history.items` → `InitialHistory::Resumed { conversation_id, history: items, rollout_path }`。**映射**：zhive 的 `read_all(source_rollout)` → 过滤出 `RolloutEntry::Item` 的 `*item` 序列。
- `core/src/thread_manager.rs:884-921` `fork_thread_with_initial_history`：计算 `forked_from_thread_id`（895-899：Resumed→源 thread id），`fork_history_from_snapshot`（901，按 ForkSnapshot 截断到 turn 边界），`spawn_thread(config, history, forked_from_thread_id, ...)` **开全新 thread id**。**映射**：zhive 新 thread id + `Thread.forked_from = Some(source)` + 新 rollout 的 `SessionHeader.parent_session = Some(source)`。
- `core/src/thread_manager.rs:589-618` `start_thread_with_options_and_fork_source(options, forked_from_thread_id)`：把 forked_from 透传进 `spawn_thread_with_source`，落 turn_metadata（turn_metadata.rs:132/148/157）+ state_db。**映射**：zhive enqueue `StorageWriteOp::ThreadUpserted{forked_from:Some}` + 新增 `StorageWriteOp::ForkHeader`。
- `core/src/thread_manager.rs:622-649` `spawn_subagent(forked_from_thread_id, options)`：subagent 复用同一 fork 路径——先 `flush_rollout()`（630，**关键：读快照前先 flush 源 thread 的待写**）→ `read_thread(include_history:true)` → `stored_thread_to_initial_history` → `start_thread_with_options_and_fork_source(.., Some(forked_from))`。
- `core/src/thread_manager.rs:1408-1439` `truncate_before_nth_user_message` + 1448+ `snapshot_turn_state`：fork 截断到「第 n 个 user message 之前」的 turn 边界（zhive 的 `up_to_item` 等价物，按 ItemId 截断）。
- 测试范本：`core/src/agent/control_tests.rs:615` `spawn_agent_can_fork_parent_thread_history_with_sanitized_items`、:916 `..fork_flushes_parent_rollout_before_loading_history`（先 flush 再读的不变量验证）。
- 关键差异：**codex fork = 每个 fork 开新 jsonl 文件，forked_from 跨文件指**（与 zhive 每 thread 一个 `<thread_id>.jsonl` 完全一致，rollout_path 推导见 mod.rs:107-111）。

**B. pi 同 thread leaf-pointer（基线 C 模型，仅保留作为「branch_summary 文本生成」+「leaf 切叶」机制的二级参考，~/Desktop/code/github/pi/packages/agent/）**：
- `harness/session/jsonl-storage.ts:226-244` `setLeafId(leafId)`：显式 append `{type:"leaf", id, parentId:currentLeafId, targetId:leafId}` 行 + 维护 currentLeafId。`leafIdAfterEntry`（同文件 `entry.type==="leaf" ? targetId : entry.id`）。**映射**：zhive `RolloutWriter::set_leaf_id`。
- `harness/agent-harness.ts:737-833` `navigateTree(targetId, {summarize})`：`phase="branch_summary"`（742）→ `collectEntriesForBranchSummary(oldLeafId,targetId)`（748）→ 可选 `generateBranchSummary(entries, {model,...})`（770）→ `session.moveTo(newLeafId, {summary})`（812）→ `phase="idle"`（finally 833）。**这是同 thread 模型**；zhive 全量把它降级为 codex 跨 thread fork 路径里「可选的 branch summary 文本生成」一步（复用 zhive compaction.rs:263 summarize 而非 pi generateBranchSummary）。
- `harness/session/jsonl-storage.ts:200-219` `static create({cwd, sessionId, parentSessionPath})`：写 `SessionHeader { parentSession: parentSessionPath }`。**映射**：zhive 新 thread 的首行 `RolloutEntry::Session { parent_session: Some(source) }`（当前 writer.rs:283 写死 None 需改）。

## approach
**全量方案 = codex「新 thread 跨 thread fork」为主干 + 基线 C 的 leaf/branch_summary/span 机制为辅。** 为什么：用户拍板档 B「真正的跨 thread fork（新 thread + forked_from + parent_session + JSONL 重放）」，基线 C 明确否决了这条（理由是踩 B2 lazy-load 硬边界）；全量做法是**同时落地 B2 fork 所需的最小重放接口**（见 crossModuleDeps），从而合法地实现 codex 模型。基线 C 已设计的 `set_leaf_id` / branch_summary span / Submission 骨架全部保留并扩展。

**完整设计（时序/拓扑）**：

【拓扑】源 thread S（已有 `S.jsonl`：Session 行 + N×Item 行 + 若干 Leaf 行）→ fork 在某历史点（按 `up_to_item: Option<ItemId>` 指定 turn/leaf 边界）→ 生成全新 thread C（新 id `thread:native/fork/<uuid>` 或 `thread:fork/<S-stem>/<n>`）→ 写新 `C.jsonl`：首行 `Session{ id:C, parent_session:Some(S), cwd, version:3 }`，随后把 S 历史中 `up_to_item` 之前的 Item 逐条重放为 `Item{thread_id:C, turn_id:<remint>, item}` 行 + 末尾一个 `Leaf{target_id}`（可选 branch summary item）→ C 注册进 ThreadStore（items_tail 装入重放的尾部窗口）→ C 的 state_db threads 行带 `forked_from=Some(S)`。

【时序】`Engine::fork_thread(source, up_to_item, summarize) ->Result<Result<ForkReply,ForkError>,EngineError>`：
1. dispatch `Submission::Fork{source_thread_id, up_to_item, summarize, new_thread_id:None}` → `EngineInner::fork_thread`。
2. `threads().get(&source)`；若内存无且 storage 无 `source.jsonl` → `ForkError::SourceNotFound`。
3. `try_set_phase_atomic(Idle, BranchSummary)`（复用 compaction.rs:107 模式）→ 失败 `ForkError::EngineBusy{current}`；广播 `PhaseChanged{Idle→BranchSummary}`（绑 source 或新 thread id）。**全程被 `info_span!("zhive.branch_summary", "session.id"=%source.0, "zhive.parent.session.id"... )` 包裹**（observability.rs:44 常量真插桩，闭合 B9 缺口）。
4. **读源历史（依赖 state-lazy-load 新接口）**：先对 source enqueue `StorageWriteOp::Flush{source}` 并等一个 ack（或在 fork 入口直接走 storage 读，因 fork 罕见、可接受短暂落后）→ `storage.replay_thread_items(&source, up_to_item)` 返回 `Vec<Item>`（实现：`read_all(rollout_path(source))` 过滤 `RolloutEntry::Item` 的 `*item`，截断到 `up_to_item`（含）所在 turn 边界；`up_to_item=None` 取全部活跃叶历史）。窗口外历史天然可读（直接读 JSONL，不受 items_tail 256 cap 限制——这正是相对 compaction 的能力升级）。
5. 分配新 thread id C；构造 `ThreadHandle::new_idle(C)`（或新增 `new_forked(C, source)` 记录 forked_from 到内存）；把重放 items 的**尾部窗口**（最后 ≤256）`push_item` 进 C 的 items_tail；`threads().write_guard().insert(C, handle)`。
6. （summarize=true 时）复用 compaction.rs:263 `summarize(provider, &replayed_items)`（在同一 branch_summary span 内）→ 生成 summary 文本；构造 `Item::AgentMessage{ id, text:"[branch summary]\n"+summary }` 追加到 C 的 items_tail 头部之后（作为 C 的开场上下文）。失败 → 回滚 phase（leave，BranchSummary→Idle）→ `ForkError::SummarizationFailed{message}`。
7. **持久化新 thread C（codex spawn_thread 等价）**：
   - 新增 `StorageWriteOp::ForkHeader{ thread_id:C, parent_session:source, cwd, created_at }` → writer 写 C.jsonl 首行 `Session{parent_session:Some(source)}`（writer 的 header_written 机制需对 C 标记，避免后续 ThreadUpserted 再写一遍 parent_session:None header）。
   - enqueue `StorageWriteOp::ThreadUpserted(Thread{ id:C, forked_from:Some(source), source:ThreadSource::User, status:Idle, ... })`（forked_from 不再硬编码 None）。
   - 逐条 enqueue `StorageWriteOp::ItemAppended{thread_id:C, turn_id:<C 的重铸 turn>, seq, item}` 把重放历史写入 C.jsonl + state_db items（这样 C 可独立 resume/rebuild）。
   - （可选 summary item 同样 ItemAppended）。
   - 末尾 enqueue `StorageWriteOp::SetLeaf{thread_id:C, target_id:Some(<last item id>)}`（基线 C 的 set_leaf_id 真写入路径，保留）+ 一次 Flush 作为 save point。
8. `leave_branch_summary`（BranchSummary→Idle CAS + PhaseChanged 广播，照 compaction.rs:177-191）。
9. 广播 `EngineEvent::ThreadForked{ source_thread_id:source, new_thread_id:C, forked_from_item:up_to_item }`（新事件，UI/observer 可见）；回 `ForkReply::Forked{ new_thread_id:C, items_replayed:n, summarized:bool }`。

**握手/重放协议（与 writer 的顺序约定）**：JSONL 是 source of truth（B3 §7.2）。Fork 读源前必须保证源的待写已落盘——通过 fork 入口先 enqueue `Flush{source}` 并等待。新 thread C 的写序严格为 `ForkHeader`（首行 Session+parent_session）→ N×`ItemAppended` → `SetLeaf` → `Flush`（fsync save point），与 writer.rs apply_turn_ended 的「JSONL 先于 SQL」不变量一致。子进程协议：本特性不涉及子进程（subagent 已是同 engine 内 fork 的近亲，见 subagent_spawn.rs；codex spawn_subagent 复用同一 fork 路径，zhive 可后续让 subagent 走 fork 而非 new_child，但本阶段保持 subagent 现状不动）。

**保留基线 C 的部分**：`RolloutWriter::set_leaf_id`（基线 files 第一项）、`StorageWriteOp::SetLeaf`、branch_summary span 真插桩 + 删 observability.rs:144-149 deferred 注释、可选 `Pre/PostBranchSummary` hook reserved 类型——全部保留。**扩展超出基线 C 的部分**：Submission 从 `BranchSummary{thread_id,target_item_id}`（同 thread）升级为 `Fork{source_thread_id, up_to_item, summarize}`（跨 thread）；新增 `StorageWriteOp::ForkHeader`；EngineInner 留存 `Arc<Storage>` 读路径；writer.rs:283 的 `parent_session:None` 改为可携带 source；state-lazy-load 新增 `replay_thread_items`。

## files

- `crates/zhive-core/src/persistence/rollout.rs` — (基线 C 保留) 新增 `pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()>`：append `RolloutEntry::Leaf{target_id: target_id.map(str::to_owned)}` 并 flush（不 fsync）。补 doctest（tempfile round-trip：写 Leaf{Some} 后 read_all 命中）。+ 新增 `pub async fn append_session_header(version,id,timestamp,cwd,parent_session: Option<&str>)` 便捷方法或直接复用 append（fork 走 ForkHeader op 时用）。
- `crates/zhive-core/src/persistence/writer.rs` — StorageWriteOp 新增两变体：`SetLeaf{thread_id, target_id: Option<String>}`（基线 C）+ `ForkHeader{thread_id, parent_session: ThreadId, cwd: String, created_at: i64}`（全量新增）。apply_op 加两分派；`apply_set_leaf`（rollout_for→set_leaf_id→sync_all 作 save point）；`apply_fork_header`（rollout_for→append Session{parent_session:Some}→标记 header_written 集合，避免后续 ThreadUpserted 再写 None header）。**改 apply_thread_upserted（274-308）：从 thread.forked_from 派生 session header 的 parent_session**（不再写死 283 的 None）——但因 ForkHeader 已先写 header，此处对已 fork 的 C 走 header_written 跳过。rebuild_state_from_rollout（479-571）：拆出 Leaf 分支记录 last_leaf_target（target_id=None 仍是 turn save point，Some 是切叶/fork，DB 不重建 item 表只 tracing::debug 记活跃叶）；Session 分支读回 parent_session→`forked_from`（509-532 现在丢弃 parent_session，需接住映射进 Thread.forked_from）。
- `crates/zhive-core/src/persistence/mod.rs` — Storage 新增公开读方法 `pub async fn replay_thread_items(&self, source: &ThreadId, up_to: Option<&ItemId>) -> StorageResult<Vec<Item>>`：`read_all(self.rollout_path(&source.0))` → 过滤 `RolloutEntry::Item{item,..}` 收集 `*item`，遇到 id==up_to 的 item 截断（含该 item 所在 turn 的边界，按 turn_id 分组取 ≤ 该 turn）。NotFound→Ok(vec![])。doc + doctest（写一个 2-item rollout 后 replay 出 2 item）。(这是 state-lazy-load topic 的接口扩展点，见 crossModuleDeps)
- `crates/zhive-core/src/engine/submission.rs` — 新增 `Submission::Fork{ source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool }`（替代基线 C 的同 thread BranchSummary 变体——全量是跨 thread）。新增 `ForkReply{ Forked{ new_thread_id: ThreadId, items_replayed: u32, summarized: bool } }` 与 `ForkError{ SourceNotFound, EngineBusy{current: EnginePhase}, ReplayFailed{message}, SummarizationFailed{message} }`（手写 impl Display+Error，照 CompactError:116-133）。SubmissionReply 加 `Fork(Result<ForkReply, ForkError>)`。
- `crates/zhive-core/src/engine/fork.rs` — 新建模块（参照 compaction.rs + subagent_spawn.rs 结构，~250 行，独立文件避免 inner.rs 超 600 行）。`impl EngineInner { pub(in crate::engine) async fn fork_thread(self:&Arc<Self>, source, up_to_item, summarize) -> Result<ForkReply,ForkError> }`：见 approach 时序 1-9。含 `leave_branch_summary`（照 compaction.rs:177-191）+ `allocate_fork_thread_id` + 模块级 `// TODO(phase2): subagent 改走此 fork 路径替代 new_child`。
- `crates/zhive-core/src/engine/inner.rs` — (1) **新增字段 `storage: Option<Arc<Storage>>`** 到 EngineInner（131 附近），构造器 new_with_hooks_tools_storage（173-203）新增 storage 参数并存入（用于 fork 读源 rollout）。(2) dispatch（350-418）加 `Submission::Fork{source_thread_id,up_to_item,summarize}` 分支调 self.fork_thread(...) 回 SubmissionReply::Fork。(3) 新增 `pub(in crate::engine) fn storage(&self) -> Option<&Arc<Storage>>` 访问器。
- `crates/zhive-core/src/engine.rs` — (1) mod 列表（34-44）加 `mod fork;`。(2) spawn_with_config（403-463）：在 PersistenceWriter::spawn 后 **不再 drop config.storage**，把 `config.storage.clone()` 传入 new_with_hooks_tools_storage 新增的 storage 参。(3) 加公开 `pub async fn fork_thread(&self, source_thread_id, up_to_item: Option<ItemId>, summarize: bool) -> Result<Result<ForkReply,ForkError>,EngineError>`（照 compact() 645-657 双层 Result + submit_with_reply）+ no_run doctest（对未知 source 返回 SourceNotFound）。(4) EngineEvent 加 `ThreadForked{source_thread_id, new_thread_id, forked_from_item: Option<ItemId>}`（event.rs）。
- `crates/zhive-core/src/engine/event.rs` — EngineEvent 加 `ThreadForked{ source_thread_id: ThreadId, new_thread_id: ThreadId, forked_from_item: Option<ItemId> }` 变体（#[non_exhaustive] 已是，加变体不破坏）。
- `crates/zhive-core/src/state/thread.rs` — (可选但建议) 新增 `pub fn new_forked(id: ThreadId, forked_from: ThreadId) -> Self`（记录内存 forked_from 形态——若把 forked_from 提升为 ThreadHandle 字段则需加 `pub forked_from: Option<ThreadId>`；否则仅持久化层带 forked_from，内存不存，fork 后 ThreadUpserted 快照里填 Some 即可，内存 handle 不必持有）。倾向：内存不加字段，仅 ThreadUpserted 快照带 forked_from:Some（最小改动），thread.rs 不动。
- `crates/zhive-core/src/observability.rs` — 删 span_literals_match_constants（144-149）的 deferred 注释块，把 `spans::BRANCH_SUMMARY` 纳入正式断言（115-149 块内加 `assert_eq!(spans::BRANCH_SUMMARY, "zhive.branch_summary")` + 注释 'engine/fork.rs: info_span!("zhive.branch_summary")'）。span_emission_tests（164+）加 `fork_opens_zhive_branch_summary_span`：用 Storage 起 engine→seed source turn→engine.fork_thread(source, None, summarize=true)→断言 recorded 含 'zhive.branch_summary'。闭合 B9 缺口。
- `crates/zhive-proto/src/hook.rs` — (可选，建议同 PR) HookEvent 加 reserved `PreBranchSummary(PreBranchSummaryInput)` / `PostBranchSummary(PostBranchSummaryInput)`（payload flatten HookEventBase + `source_thread_id` + `entries_count`），对齐 decision-diffs §1.7/§1.14。本阶段仅加类型 + serde round-trip doctest（照现有 PreCompact），dispatch 接线推后。HookEvent 是 `#[serde(tag="hook_event_name")]` + #[non_exhaustive]，加变体不破坏 wire。

## newTypes
- Submission::Fork { source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool }
- enum ForkReply { Forked { new_thread_id: ThreadId, items_replayed: u32, summarized: bool } }
- enum ForkError { SourceNotFound, EngineBusy { current: EnginePhase }, ReplayFailed { message: String }, SummarizationFailed { message: String } } // + impl fmt::Display + std::error::Error（手写，照 CompactError）
- SubmissionReply::Fork(Result<ForkReply, ForkError>)
- impl RolloutWriter { pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()> }
- StorageWriteOp::SetLeaf { thread_id: ThreadId, target_id: Option<String> }
- StorageWriteOp::ForkHeader { thread_id: ThreadId, parent_session: ThreadId, cwd: String, created_at: i64 }
- impl Storage { pub async fn replay_thread_items(&self, source: &ThreadId, up_to: Option<&ItemId>) -> StorageResult<Vec<Item>> }
- impl EngineInner { storage: Option<Arc<Storage>> 字段; pub(in crate::engine) fn storage(&self) -> Option<&Arc<Storage>>; pub(in crate::engine) async fn fork_thread(self: &Arc<Self>, source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool) -> Result<ForkReply, ForkError> }
- impl Engine { pub async fn fork_thread(&self, source_thread_id: ThreadId, up_to_item: Option<ItemId>, summarize: bool) -> Result<Result<ForkReply, ForkError>, EngineError> }
- EngineEvent::ThreadForked { source_thread_id: ThreadId, new_thread_id: ThreadId, forked_from_item: Option<ItemId> }
- EngineInner::new_with_hooks_tools_storage 签名新增 storage: Option<Arc<Storage>> 参数（已 #[expect(too_many_arguments)]，再加一个不破坏）
- (可选) HookEvent::PreBranchSummary(PreBranchSummaryInput) / PostBranchSummary(PostBranchSummaryInput) + 对应 struct（flatten HookEventBase + source_thread_id + entries_count）

## redlineImpact
**不触红线。**
- **无新 crate 依赖**：复用既有 tokio/tokio-util/serde/serde_json/thiserror/tracing/futures/llmsdk；replay 复用 rollout::read_all（既有 tokio::fs）；summarize 复用 compaction.rs:263（同 DynLanguageModel provider trait，不平行造轮子）。fork 读源 JSONL 复用 Storage::rollout_path（mod.rs:107）+ read_all（rollout.rs:145）。
- **无 unsafe。**
- **无非测试 unwrap()/expect()**：所有错误走 `?` + 新 thiserror 风格枚举（ForkError 手写 Display+Error 与 CompactError 一致，保持本模块风格而非 derive thiserror）；replay 截断、id 重铸全用 saturating/map_or（照 writer.rs:578-583 unix_now）。
- **公开 API doc + doctest**：Engine::fork_thread（no_run，对未知 source 返回 SourceNotFound）、RolloutWriter::set_leaf_id（tempfile round-trip）、Storage::replay_thread_items（写 rollout 后 replay）必须带 doctest。
- **wire 兼容**：Submission/SubmissionReply/EngineEvent/HookEvent 均 #[non_exhaustive]，加变体不破坏（D-012「至少 14」+ decision-diffs §1.10 同理）。HookEvent serde tag 不变。
- **单文件 <600 行**：fork.rs 新建独立模块（~250 行）避免 inner.rs 超限；writer.rs 当前 782 行**已超 600 软上限**，新增两 op 的 apply_* 应抽到 writer 子模块或同步评估拆分（在 PR 说明）。
- **feature 门控**：observability 的 OTel 相关（noop_tracer_provider 用 opentelemetry_sdk）已是既有依赖，branch_summary span 是纯 tracing::info_span! 字面量，不引新 feature。
- **DELIBERATE 标注**：EngineInner 留存 Arc<Storage> 会让纯内存 engine（storage:None）该字段为 None；fork 在 storage:None 下直接返回 SourceNotFound（无 source rollout 可读）——需在 doc 注明「跨 thread fork 需配置 storage」。

## crossModuleDeps
- **与 state-lazy-load（强耦合，本特性的硬前置）**：跨 thread fork 的「从 S.jsonl 重放历史→items」**不是** B2 占位 trait 的 `load_items_page(thread_id,turn_id,offset,limit)`（那是按 turn 分页、给 UI lazy load 用）。fork 需要的是**整源 thread 一次性重放到 ItemId 边界**。请 state-lazy-load topic 扩大实现范围，至少提供以下精确接口之一（按优先级）：(A) 最小够用：`Storage::replay_thread_items(&self, source: &ThreadId, up_to: Option<&ItemId>) -> StorageResult<Vec<Item>>`（实现 = read_all(rollout_path(source)) 过滤 Item + 截断到 up_to）——本设计 files 已把它放在 persistence/mod.rs，可由 state-lazy-load topic 落地实现体；fork.rs 仅消费。(B) 若 state-lazy-load 要做通用 lazy load，则 fork 复用其 `rebuild_thread_from_jsonl(source) -> (Thread, Vec<Turn>/Vec<Item>)` 返回完整历史，fork 再自行按 up_to 截断。**关键约定**：该接口必须能读 items_tail 256 窗口之外的历史（直接读 JSONL，不依赖内存窗口）——这正是 fork 相对 compaction（只操作 items_tail）的能力升级，也是基线 C 否决跨 thread fork 的那条硬边界，现由此接口闭合。
- **与 B3 persistence**：依赖 `RolloutEntry::Leaf.target_id`（rollout.rs:54）+ `RolloutEntry::Session.parent_session`（rollout.rs:39，当前 writer.rs:283 写死 None 需改）。新增 StorageWriteOp::SetLeaf + ForkHeader 须与 writer 现有「Leaf=turn 完成标记(target_id=None)」语义并存：约定 target_id=None 仍 turn save point，Some=fork/切叶；ForkHeader 写的 Session 行须更新 writer 的 header_written 集合（writer.rs:166,276-296）避免后续 ThreadUpserted 重写 header。rebuild（writer.rs:479-571）须把 Session.parent_session 接回 Thread.forked_from（当前 509-532 丢弃）。
- **与 B9 tracing**：本方案落地 `zhive.branch_summary` 真插桩（fork.rs 的 info_span），消除 observability.rs:144-149 deferred 注释——是 B9 缺口『branch_summary span 无插桩』的直接闭合点；必须同 PR 改 observability 测试（span_literals_match_constants + span_emission_tests），否则常量断言与真插桩不同步。span 字段建议 `session.id`=新 thread C + `zhive.parent.session.id`=source（复用 fields::PARENT_THREAD_ID observability.rs:68，与 subagent span 一致）。
- **与 #8『client-native + fork』任务（task #8）**：本设计就是 #8 fork 部分的全量实现。client-native（server 层）需暴露 fork 的 JSON-RPC method（如 thread/fork），其参数映射 Engine::fork_thread(source, up_to_item, summarize)，返回 new_thread_id。server topic 须在 Submission/SubmissionReply round-trip 中加 Fork case。
- **与 A4 hook**：PreBranchSummary/PostBranchSummary 若加，须与 HookHost::dispatch 的 14+ 事件注册表对齐（decision-diffs §1.7 已 reserved）；hook 失败按 compaction.rs:dispatch_compact_hook（196-241）的『log-and-proceed 内部维护』语义，不能让 hook 否决 fork。本阶段仅加类型+serde 测试，dispatch 接线推后。
- **与 subagent（弱耦合，仅 TODO）**：codex spawn_subagent（thread_manager.rs:622）复用 fork 路径；zhive 当前 subagent 走 ThreadHandle::new_child（空历史，subagent_spawn.rs:177）。本阶段**不改 subagent**，仅在 fork.rs 留 `// TODO(phase2): unify subagent spawn onto fork path（forked subagent = 带父历史的 child）`。

## tests
- 单测(rollout.rs)：set_leaf_id 写 Leaf{target_id:Some} 后 read_all round-trip 出该 target_id（照 append_and_read_round_trip:171）
- 单测(mod.rs)：replay_thread_items —— 写 Session+2×Item 的 rollout → replay(source, None) 返回 2 个 Item；replay(source, Some(item0_id)) 截断到 1 个（边界精确）；source 无文件 → Ok(vec![])
- 单测(fork.rs)：fork_thread 在 Idle + storage 配置下 summarize=false → ForkReply::Forked{items_replayed:n}，新 thread 注册进 ThreadStore 且 items_tail 含重放尾部，phase 回 Idle（照 compaction.rs:356）
- 单测(fork.rs)：fork_thread 非 Idle（先 CAS 到 Turn）→ ForkError::EngineBusy（照 run_compaction_busy_when_not_idle:411）
- 单测(fork.rs)：未知 source（内存无 + storage 无 rollout）→ ForkError::SourceNotFound
- 单测(fork.rs)：fork 后新 thread 的 ThreadUpserted 快照 forked_from==Some(source)（断言 enqueue 的 op 或读 state_db get_thread(new).forked_from）
- writer e2e(writer.rs)：enqueue ForkHeader → JSONL 首行 Session{parent_session:Some}；enqueue SetLeaf → 末行 Leaf{target_id:Some}（照 writer_applies_ops_and_persists_items:691）
- rebuild e2e(writer.rs)：写带 parent_session 的 C.jsonl → rebuild_state_from_rollout → get_thread(C).forked_from==Some(source)（验证 Session.parent_session→forked_from 接回）
- span 集成测试(observability.rs span_emission_tests)：fork_opens_zhive_branch_summary_span（Storage 起 engine→seed source→fork(summarize=true)→SpanCapture 断言含 zhive.branch_summary）
- doctest：Engine::fork_thread（no_run，未知 source→SourceNotFound）；RolloutWriter::set_leaf_id（tempfile）；Storage::replay_thread_items（写后 replay）
- (若做 hook)doctest：PreBranchSummary serde round-trip（照 hook.rs PreCompact 现有 doctest）
- 回归：summarize=true 须用 ScriptedModel（compaction.rs:320 inner_with_summary 范式）避免真实网络

## risks
中。主要风险：

(1) **EngineInner 留存 Arc<Storage> 的影响面**：当前 spawn_with_config 消费掉 config.storage（engine.rs:409-419），改为 clone 留存会让所有现有「storage:None」测试路径仍 None（无回归），但需确认 PersistenceWriter::spawn 后 Arc 引用计数 +1 的生命周期正确（writer 任务持一份，inner 持一份，shutdown 时 inner 那份随 engine drop——不影响 writer drain）。

(2) **fork 读源前的 flush 时序（codex control_tests.rs:916 验证的不变量）**：若源 thread 有未 flush 的待写 item（BufWriter 缓冲，writer.rs:107 append 只 flush 不 fsync，apply_turn_ended 才 sync_all），fork 直接 read_all 可能漏掉最新 item。缓解：fork 入口先 enqueue Flush{source} 并短暂 await（或文档约定 fork 只在源 turn 完成后调用，复用 phase Idle 前置——源若在 Turn，整个 engine 非 Idle，fork 本就 EngineBusy 失败，天然规避大部分场景；但跨 thread 时源可能是另一已完成 thread，其 BufWriter 仍可能有缓冲——需 Flush 握手）。

(3) **新 thread id 重铸 turn_id/item_id 的唯一性**：重放 source 的 item 写入 C.jsonl 时 turn_id 必须重铸为 C 的命名空间（否则 C.jsonl 里出现 source 的 turn_id，rebuild 会污染 turn_index）。约定：fork 把所有重放 item 归入单个合成 turn `turn:{C}/forked` 或保留原 turn 结构但前缀替换。item.id 同理（codex 用 sanitize，zhive 可用 `item:{C}/replay/{seq}` 重铸 id——但这会破坏 item 内部引用如 tool_call_id↔tool_result 关联）。**倾向保留原 item id**（item id 全局唯一前缀已含 turn，重放不改 id，turn_index 用合成 turn）——需在 fork.rs 注释 DELIBERATE。

(4) **Leaf{target_id=None}(turn 标记) vs Some(切叶/fork) 共用 enum**：rebuild 必须不把 fork 切叶误当 turn 完成——已在 crossModuleDeps 约定语义。

(5) **branch_summary 与 compaction 都要 phase 转换 + 都可能截断/重组 items**：二者互斥靠 phase CAS 天然保证（compaction 走 Idle→Compaction，fork 走 Idle→BranchSummary，互不相容）。

(6) **writer.rs 已 782 行**：新增 op 处理可能进一步超标，须评估拆 writer 子模块（PR 说明）。

回滚风险低：fork 失败路径照 compaction.rs:133 回滚 phase；新 thread C 若中途失败，C.jsonl 可能半写——但 JSONL append-only + Leaf 缺失=未完成 fork，rebuild 可识别（C 无 Leaf=不完整，标记 needs_rebuild 或丢弃）。

## recommendation
**实现顺序（自底向上，每步独立可测）**：
1. **rollout.rs**：set_leaf_id + append_session_header 便捷方法 + 单测（基线 C 最底层，零依赖）。
2. **persistence/mod.rs**：replay_thread_items + 单测（state-lazy-load 接口的最小落地；若 state-lazy-load topic 要做通用 rebuild，此处改为薄封装其接口）。
3. **writer.rs**：StorageWriteOp::SetLeaf + ForkHeader 两 op + apply_* + rebuild 的 Leaf/Session(parent_session→forked_from) 分支 + writer e2e/rebuild e2e 测试。
4. **submission.rs**：Submission::Fork + ForkReply + ForkError + SubmissionReply::Fork。
5. **inner.rs**：EngineInner 加 storage 字段 + 构造器参数 + storage() 访问器 + dispatch Fork 分支；engine.rs spawn_with_config 改为 clone 留存 storage。
6. **fork.rs**：核心 fork_thread 逻辑（复用 compaction 骨架 + subagent_spawn 的新 thread 注册范式，可大量 copy-adapt）+ 全部单测。
7. **engine.rs + event.rs**：公开 Engine::fork_thread + doctest + EngineEvent::ThreadForked。
8. **observability.rs**：删 deferred 注释 + BRANCH_SUMMARY 纳入断言 + span 集成测试（闭合 B9）。
9. **(可选但建议) hook.rs**：Pre/PostBranchSummary reserved 类型 + serde 测试（对齐 decision-diffs §1.7，避免后续 wire break）。

**与基线设计如何合并**：
- **保留基线 C 全部 files**：set_leaf_id（files#1）、StorageWriteOp::SetLeaf（files#2）、branch_summary span 真插桩 + 删 deferred 注释（files#10）、可选 Pre/PostBranchSummary（files#11）——这些在全量里原样保留。
- **扩展基线 C 三处**：(a) Submission 从同 thread `BranchSummary{thread_id,target_item_id}` 升级为跨 thread `Fork{source_thread_id,up_to_item,summarize}`；(b) 新增 `StorageWriteOp::ForkHeader` + writer 写 Session.parent_session（基线 C 不动 writer.rs:283）；(c) EngineInner 留存 Arc<Storage> 读路径（基线 C 不需要，因同 thread 只操作内存 items_tail）。
- **基线 C 明确推后、本全量实现的三项**：跨 thread fork（新 thread+rollout 重放+forked_from 写入+SessionHeader.parent_session）= 全部落地；从 JSONL 历史定位窗口外 target = 由 replay_thread_items 直接读 JSONL 实现（不受 256 窗口限制）；唯一仍推后的是 PreBranchSummary hook 的实际 cancel 语义（本阶段仅 reserved 类型）。

**判定全量「生产可用」的标准**：EnginePhase::BranchSummary 有真实触发路径（fork 走 Idle→BranchSummary→Idle）+ zhive.branch_summary span 真插桩 + Leaf.target_id 与 Session.parent_session 不再是死字段 + forked_from 在 fork 路径真写入 + 新 thread 可独立 resume/rebuild（C.jsonl 自包含历史）。这五条全部满足即达档 B。

**给 state-lazy-load topic 的明确请求**：请落地 `Storage::replay_thread_items(source, up_to) -> Vec<Item>`（或等价的 rebuild_thread_from_jsonl），且必须能读 items_tail 256 窗口之外的历史（直接读 JSONL）——这是 fork 跨 thread 的硬前置，也是基线 C 当初否决跨 thread fork 的那条边界。
