# B7 PendingSessionWrites buffer + flush

## harnessRef
pi agent-harness.ts:174 (字段声明) / :439-481 (flushPendingSessionWrites + prepareNextTurn save point #1) / :483-510 (turn_end save point #2, agent_end save point #3) / :552-600 (executeTurn finally save point #4) / :669-679 (appendMessage 智能分发)

## approach
**方案：在 ThreadHandle 持有 per-thread PendingSessionWrites buffer，由 run_turn / compaction / lifecycle 在 5 个 save point 调 flush，flush 内部将 PendingSessionWrite 变体转换为对应的 StorageWriteOp 发向现有 PersistenceWriter。不修改 PersistenceWriter/StorageWriteOp 底层。**

理由：
1. PendingSessionWrites 是 per-thread 数据（每线程独立），放在 ThreadHandle（`active_turn: Mutex<Option<ActiveTurn>>`旁）语义最清晰，避免 EngineInner 全局 state 膨胀。
2. StorageWriteOp 是 JSONL+SQL 写入的底层协议，已稳定；不上移 Session trait（Phase 1 无 session abstraction），而是直接在 flush 内把 PendingSessionWrite 翻译为对应 StorageWriteOp variant 或新增 variant（model_change / label 等），通过 enqueue_storage_op 投递。
3. 智能分发（phase=Idle 直发，否则入 buffer）放在 PendingSessionWrites::push_or_enqueue 方法内，调用点不重复判断。

被否决的备选：
- 上层加 Session trait：Phase 1 无 Session abstraction，加 trait 只为这一个功能是过度设计，增加接口面。
- 放在 EngineInner：全局 engine 只有一个，但 thread-level session 元数据（label、session_name）是 per-thread 的，放 EngineInner 需要额外 HashMap<ThreadId, ...>，不如直接放 ThreadHandle。
- 修改 PersistenceWriter：StorageWriteOp 已稳定且经测试，仅扩展 variant 即可，不改写 writer 主循环。

## files

- `crates/zhive-core/src/state/pending_writes.rs` — 新建文件。定义 pub enum PendingSessionWrite（8 个 variant，Phase 1 可用的：Item/ModelChanged/SessionInfo/Leaf；ThinkingLevelChange/Label/Custom/CustomMessage 占位但不挂 StorageWriteOp 实装，因为 StorageWriteOp 尚无对应 variant）。定义 pub(crate) struct PendingSessionWrites { queue: VecDeque<PendingSessionWrite> }，实现 push_or_enqueue(phase, write) 和 flush(enqueue_fn) 两个方法。flush 按 variant 转换为 StorageWriteOp 并通过传入的 enqueue 闭包发出；失败立即返回 Err，已 drain 的不回填（对齐 Pi 行为）。提供 is_empty()/len() 辅助方法。每个 public 类型和方法须带 doc comment + doctest。
- `crates/zhive-core/src/state.rs` — 新增 pub mod pending_writes; 声明，并在 pub use 处导出 pending_writes::PendingSessionWrites（pub(crate)），使 engine 子模块可通过 crate::state::PendingSessionWrites 访问。
- `crates/zhive-core/src/state/thread.rs` — 在 ThreadHandle 结构体新增字段 pub(crate) pending_session_writes: std::sync::Mutex<PendingSessionWrites>。在 new_idle / with_capacity / new_child 三处构造器中初始化为 Mutex::new(PendingSessionWrites::new())。新增 pending_writes_lock() 辅助方法（同 injection_lock 模式，recover from poison）。
- `crates/zhive-core/src/persistence/writer.rs` — 在 StorageWriteOp 枚举新增两个 variant（Phase 1 实装的 session 元数据写入）：ModelChanged { thread_id, provider: String, model_id: String } 和 SessionNameSet { thread_id, name: String }。在 apply_op / run_writer 处添加对应 match arm，写入 JSONL RolloutEntry::Session 的 metadata 字段（或追加为新 metadata 行）；SQL 端 best-effort 更新 threads 表的 model_provider / name 列。ThinkingLevelChange / Label / Custom / CustomMessage 的 StorageWriteOp variant 推迟到 B5/Phase2，pending_writes.rs 的 flush 对这些 variant 只 emit warn log 并 skip（不丢失 in-memory 数据，仅不落盘）。
- `crates/zhive-core/src/engine/turn.rs` — 在 run_turn_inner 函数的 5 个位点调用 flush：(1) steer drain 循环结束后（save point #1，prepare_next_turn 语义）；(2) stream loop 结束后 fold.finish() 之后、failure/cancel 检查之前（save point #4，finally 语义）；(3) 每次 tool dispatch PHASE 3 全部 commit 之后、continue outer 前（inline save point）。flush 调用形式：let mut pw = handle.pending_writes_lock(); let _ = pw.flush(|op| inner.enqueue_storage_op(op));（flush 失败 warn log，不 abort turn——persistence 是 best-effort）。
- `crates/zhive-core/src/engine/lifecycle.rs` — 在 finish_turn 方法内，TurnCompleted 广播之后、StorageWriteOp::TurnEnded 入队之前，新增 save point #2/3：let mut pw = handle.pending_writes_lock(); let had_pending = !pw.is_empty(); let _ = pw.flush(|op| self.enqueue_storage_op(op)); 之后 emit EngineEvent::SavePoint { thread_id, had_pending_mutations: had_pending }（同 Pi :497-499）。在 cancel_turn 方法内（abort 路径）：根据 B7 §5.4 决策——abort 路径保留 buffer 内容不 flush，下次 phase 回 Idle 时由 save point #5 触发。在 finish_turn 的 phase 转回 Idle 的位点（try_set_phase_atomic 成功后）加 save point #5：先 flush pending_session_writes，再 emit PhaseChanged。
- `crates/zhive-core/src/engine/event.rs` — 在 EngineEvent 枚举新增 SavePoint { thread_id: ThreadId, had_pending_mutations: bool } variant（对应 Pi :499 的 save_point event），带 doc comment。

## newTypes

- pub enum PendingSessionWrite { Item { thread_id: ThreadId, turn_id: TurnId, seq: i64, item: Box<Item> }, ModelChanged { thread_id: ThreadId, provider: String, model_id: String }, SessionInfo { thread_id: ThreadId, name: Option<String> }, Leaf { thread_id: ThreadId }, ThinkingLevelChange { level: u8 }, Label { target_id: ItemId, label: String }, Custom { custom_type: String, data: serde_json::Value }, CustomMessage { custom_type: String, content: String } }  — crates/zhive-core/src/state/pending_writes.rs (新建)
- pub(crate) struct PendingSessionWrites { queue: VecDeque<PendingSessionWrite> }  — crates/zhive-core/src/state/pending_writes.rs
- impl PendingSessionWrites { pub fn new() -> Self; pub fn push_or_enqueue(&mut self, phase: EnginePhase, write: PendingSessionWrite, enqueue: impl Fn(PendingSessionWrite)); pub fn flush(&mut self, enqueue: impl Fn(StorageWriteOp)) -> Result<usize, PendingFlushError>; pub fn is_empty(&self) -> bool; pub fn len(&self) -> usize; }  — crates/zhive-core/src/state/pending_writes.rs
- StorageWriteOp::ModelChanged { thread_id: ThreadId, provider: String, model_id: String }  — writer.rs 新 variant
- StorageWriteOp::SessionNameSet { thread_id: ThreadId, name: String }  — writer.rs 新 variant
- EngineEvent::SavePoint { thread_id: ThreadId, had_pending_mutations: bool }  — event.rs 新 variant

## redlineImpact
无新增 crate dependency。所有实现使用现有依赖：tokio / std::sync::Mutex / VecDeque / zhive-proto domain types / tracing。
flush 使用 `?` + thiserror 错误传播（PendingFlushError 用 thiserror 定义，包装 StorageError 或 channel SendError）。
无 unsafe。非测试代码无 unwrap/expect（Mutex poison 用 into_inner 恢复，同已有 phase_lock / injection_lock 模式 — crates/zhive-core/src/state/thread.rs:167-172）。
ThreadHandle 新字段 pending_session_writes 在 new_idle/with_capacity/new_child 三处初始化，不影响公开 API 形状（字段 pub(crate)）。
StorageWriteOp 是 #[non_exhaustive]，新增 variant 不破坏已有 match（run_writer apply_op 的 match 需新增 arm，编译期强制）。

## crossModuleDeps

- state-lazy-load（B2）：ThreadHandle 新增字段须在 B2 的 lazy load 构造路径中初始化（B2 将来可能重建 ThreadHandle，需同步加 pending_session_writes: Mutex::new(PendingSessionWrites::new())）。协调约定：pending_writes 字段在 new_idle/with_capacity/new_child 三个构造器中全部初始化，B2 如果 fork ThreadHandle 构造必须走这三条路，否则编译时缺字段。
- permission-suspend-resume（B6）：B6 的 Defer 路径会把 turn 挂起并切 phase 到某个 suspended 态，恢复时切回 Turn。这两次 phase 转换都经过 lifecycle.rs；若 B6 新增 phase 态，则 save point #5（phase→Idle 前 flush）的 match 需覆盖新 phase 到 Idle 的转换。协调约定：save point #5 挂在 try_set_phase_atomic(*, Idle) 成功的共用代码路径上（lifecycle.rs:finish_turn 和 cancel_turn 都走这个 CAS），不写 phase 特定 match，因此 B6 新增 phase 不需要修改 pending_writes flush 触发点。
- hook-host（B5）：B5 的 PostToolUse hook dispatch 可能产生 session 元数据写（如 Label/SessionInfo），这些写入应通过 handle.pending_writes_lock().push_or_enqueue(phase, write) 而非直接 enqueue_storage_op，否则绕过 buffer 保护。协调约定：凡 hook 产出的 session 元数据写入，一律走 PendingSessionWrites::push_or_enqueue；工具执行本身产出的 Item 写入（StorageWriteOp::ItemAppended）已在 turn.rs PHASE 3 按现有路径直接发送，不改。
- engine/inner.rs enqueue_storage_op：flush 闭包参数用 impl Fn(StorageWriteOp) 形式接受 inner.enqueue_storage_op，不持有 Arc<EngineInner>，避免 lifetime/借用冲突。flush 调用点的模式：{ let mut pw = handle.pending_writes_lock(); let _ = pw.flush(|op| self.enqueue_storage_op(op)); }（self 是 &EngineInner，闭包 capture by ref，无循环借用）。

## tests

- PendingSessionWrites::push_or_enqueue when phase=Idle 直接调用 enqueue（buffer 仍空）
- push_or_enqueue when phase=Turn 入 buffer（enqueue 不被调用）
- flush 按入队顺序发出 StorageWriteOp，返回 count
- flush 在首个写入失败时停止，已 drain 的不回填（模拟 channel Closed 错误）
- flush 空 buffer 返回 Ok(0)
- ThreadHandle::new_idle 构造后 pending_session_writes.lock().is_empty() == true
- lifecycle::finish_turn 触发 SavePoint event with had_pending_mutations=true 当 buffer 非空
- lifecycle::finish_turn save point #5（phase→Idle 前）先 flush，后 emit PhaseChanged
- turn::run_turn_inner steer drain 后（save point #1）buffer 已清空
- abort 路径 cancel_turn 不 flush buffer（buffer 内容保留到下次 Idle）
- StorageWriteOp::ModelChanged / SessionNameSet 在 apply_op 中正确更新 threads 行（writer e2e test）
- doctest for PendingSessionWrites::push_or_enqueue 展示 phase=Idle 直发 vs Turn 入 buffer 的对比

## risks
1. flush 闭包形式 impl Fn(StorageWriteOp) 在 run_turn_inner 中闭包 capture &Arc<EngineInner> via self（实际是 &Arc<Self> 方法调用），需确认 borrow checker 接受。如果 lifetime 冲突，改为先 collect Vec<StorageWriteOp> 再批量 enqueue（不影响语义）。
2. push_or_enqueue 的 phase 参数每次调用都需要读锁 EngineInner::phase（std::sync::Mutex），但 run_turn_inner 里调用频率不高（仅 steer drain 和 finally 处），不是性能热路径。
3. PendingSessionWrite::Item 内嵌 thread_id/turn_id/seq，使 variant 较重（~100B per entry），但 Phase 1 buffer 在 turn 期间只积累少量 session 元数据条目（非 item stream），实际不是 Item variant 的主用途——主用途是 ModelChanged/SessionInfo/Leaf 这三类轻量 variant，问题不大。
4. StorageWriteOp 新增 variant 后，writer.rs 的 apply_op match 必须新增 arm，否则编译失败；由于是 non_exhaustive enum，外部 crate 有 _=>{} 兜底但 internal match 无兜底——这是正确行为，编译期强制完备性。
5. 已有 writer_applies_ops_and_persists_items 等测试覆盖现有 variant；新增 variant 不破坏已有测试（非穷举 match），但需在 writer.rs 的 mod tests 内为 ModelChanged/SessionNameSet 补充新 test case。

## recommendation
实现顺序：
1. 先建 pending_writes.rs，定义 PendingSessionWrite / PendingSessionWrites / PendingFlushError。此步骤无跨文件依赖，可独立编译验证（cargo check -p zhive-core --lib）。
2. 修改 state.rs 导出 + state/thread.rs 加字段（构造器 + pending_writes_lock()）。
3. 修改 persistence/writer.rs 新增 StorageWriteOp variant + apply_op arm（ModelChanged/SessionNameSet），同步跑 writer tests。
4. 修改 engine/event.rs 新增 SavePoint variant。
5. 修改 engine/lifecycle.rs 插入 save point #2/3（finish_turn）和 save point #5（phase→Idle 前），abort 路径不 flush。
6. 修改 engine/turn.rs 插入 save point #1（steer drain 后）和 save point #4（stream finally）。
7. 全量 cargo nextest run -p zhive-core + fmt/clippy。

范围建议：Phase 1 仅实装 Item/ModelChanged/SessionInfo/Leaf 四类 StorageWriteOp 对应 variant；ThinkingLevelChange/Label/Custom/CustomMessage 的 PendingSessionWrite variant 保留但 flush 对它们只 warn+skip（不落盘），因为 StorageWriteOp 尚无对应 JSONL/SQL 路径，强行落盘意义不大，且 JSONL 的 RolloutEntry 枚举也只有 Session/Item/Leaf 三种——强行写 label/custom 会引入新的 RolloutEntry variant，超出本 task 范围。
