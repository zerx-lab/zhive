# B2 lazy-load 内存模型（TurnHistoryBuffer/ThreadEvent/ThreadStorage 整合）+ session_index

## currentState
实际代码已远超 deliverable 草图，且关键技术选型与 B2/B3 草图不同，方案必须基于现状而非草图：

1. 持久化已用 sqlx 不是 rusqlite（decision-diffs.md:37-39 R-7 选方案 d 是 rusqlite 时代的，已被 sqlx 推翻）。`StateDb` 用 `SqlitePool`（state_db.rs:42-66），WAL+NORMAL+FK 已配（state_db.rs:57-62）。Storage::open 并发开 4 库（mod.rs:75-95）。

2. `StorageWriteOp` 已存在且字段名与草图不同：writer.rs:48-95 有 `ThreadUpserted(Box<Thread>)/TurnStarted{thread_id,turn_id,started_at}/ItemAppended{thread_id,turn_id,seq:i64,item:Box<Item>}/TurnEnded{...status,error,completed_at,duration_ms}/Flush{thread_id}`，`#[non_exhaustive]`。草图的 `AppendItem/ThreadMetadata/FlushBarrier{ack:oneshot}` 命名不存在——不要新造，扩展现有 enum。

3. `PersistenceWriter` actor 已实现 write-through（writer.rs:204-235）：唯一 consumer，per-thread `RolloutWriter`，fsync 仅在 TurnEnded/Flush（writer.rs:369-415, 433-444），shutdown drain（writer.rs:218-229）。引擎侧 `enqueue_storage_op` 非阻塞 try_send（inner.rs:274-288）。

4. 内存模型现状是 per-handle `items_tail: RwLock<VecDeque<Item>>` + `items_tail_capacity`（thread.rs:38-41, 179-185 push_item 截断兜底），无 Turn 维度、无 TurnItemsView 三态、无 lazy_unloaded_count、无 write_queue。`ThreadStore` 是 `RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>`（thread.rs:343-345）。

5. 无 thread 级 `ThreadEvent`——只有 engine 级 `EngineEvent` broadcast（event.rs:38-159，已含 TurnStarted/ItemAppended/ItemDelta/TurnCompleted/TurnFailed/PhaseChanged/Usage/SubagentCompleted 等），由 `EngineInner.events_tx`（inner.rs:83, 211-213）单一 fan-out，`Engine::subscribe()`（engine.rs:508）暴露。

6. 无 `ThreadStorage` trait——持久化是具体类型 `Storage`/`StateDb`（非 trait，无 mock 注入点）。`EngineConfig.storage: Option<Arc<Storage>>`（engine.rs:187），spawn_with_config 据此 spawn writer（engine.rs:403-416）。

7. 无 session_index.jsonl（mod.rs 整文件无），无独立多文件 rebuild。已有 per-file `rebuild_state_from_rollout(state, rollout_path)`（writer.rs:479-571，逐行 replay Session/Item，best-effort 标 turn Completed）。RolloutEntry 仅 3 case：Session/Item/Leaf（rollout.rs:28-60），无 turn_start/turn_end/branch_summary/leaf-with-parent。Leaf 当前总写 target_id:None（writer.rs:384）。

8. 无 thread/read、thread/list、turn/get_items 等读 handler——server/handlers.rs 只注册写/控制类（handlers.rs:49-97），读路径在 server 层完全缺失。

9. migration 文件实际名为 0001_init.sql（不是草图的 0001_threads.sql），state schema 是 threads/turns/items 三表（state/0001_init.sql:8-47），无 forked_from 索引外的 turn_index 单独表（turns 表已含 thread_id+started_at 索引）。

10. domain 已就位且复用：Thread{turns:Vec<Turn>}(domain.rs:124-155)、Turn{items,items_view:TurnItemsView,status,...}(domain.rs:218-241)、TurnItemsView{NotLoaded/Summary/Full}(domain.rs:248-256, Full 是 default)。D-006 单源已落地，B2 直接复用这些类型。

## harnessRef
codex ~/Desktop/code/github/codex/codex-rs/rollout/src/session_index.rs:16-80（SESSION_INDEX_FILE 常量 + SessionIndexEntry{id,thread_name,updated_at:Rfc3339} + append_thread_name/append_session_index_entry append-only OpenOptions append+create + find_thread_name_by_id 用 spawn_blocking 倒序扫——zhive 借鉴 append-only + 倒序最新胜，但用 i64 unix-seconds 替 Rfc3339 与现有 created_at 一致，且全 async tokio::fs 不引 spawn_blocking）。codex thread-store/src/store.rs:38-65,98-103（ThreadStore trait：create/resume/append_items/load_history/list_items 分页——借鉴 trait 方法集做 ThreadStorage trait 抽象，使 mock 可注入）。pi packages/agent/src/harness/session/jsonl-storage.ts:109-111,250-259（leafIdAfterEntry：普通 append leaf 隐式=entry.id，仅 fork 显式写 leaf——zhive 现在总写 None，本阶段沿用 None 不做 fork tree，fork 推到 client-native 任务#8）。本任务现有 writer.rs/state_db.rs 本身就是最权威 harness。

## approach
分四块，全部"扩展现有结构"而非新造平行体系。被否决备选见各块末。

【块1 TurnHistoryBuffer——替换 items_tail 为 turn 维度滚动 window + lazy 三态】
在 state/ 下新增 `turn_buffer.rs`，定义 `TurnHistoryBuffer`：
- `active: Option<Turn>`（schema 投影，与 ActiveTurn 运行时态对偶，靠 turn_id 同步）
- `completed: VecDeque<Turn>`（按 started_at 单调）
- `in_memory_turn_cap: usize`（默认常量 IN_MEMORY_TURN_CAP=50）
- `lazy_unloaded_count: usize`
方法（全同步短操作，无 await，配 std::sync::Mutex 或保持 tokio Mutex 见下）：`start_turn(turn:Turn)`、`push_item(item:Item)`（推到 active.items）、`finish_turn(status,completed_at,duration_ms)`（active.take → push_back completed → 调 enforce_cap）、`enforce_cap()`（completed.len()>cap 时把最老 turn 的 items 清空 + items_view=Summary/NotLoaded + lazy_unloaded_count+=1）、`recent_turns(offset,limit)` 读视图。
ThreadHandle 改造：删 `items_tail/items_tail_capacity`，新增 `history: Arc<Mutex<TurnHistoryBuffer>>`（保留 tokio::sync::Mutex，因 push_item 调用点在 turn.rs 都是 async 上下文，且需与 active_turn 锁顺序兼容）。`push_item` 改为 push 到 active turn 的 items；保留 `item_ids()` 兼容（遍历 active+completed）。turn.rs/lifecycle.rs 所有 `handle.push_item` 调用点签名不变（仍 async），内部改走 history.lock()。turn.rs:683 `handle.items_tail.read().await.len()` 改 history 的 item 计数方法。prompt 构建（inner.rs doc 提到从 items_tail 映射 Prompt）的实际读点需同步改为遍历 history。
被否决：直接在 ThreadHandle 上加 turns 字段——会和 items_tail 并存造成双源，违反单一存储位置不变量。

【块2 ThreadEvent——thread 级 broadcast，与 EngineEvent 并列两层】
不替换 EngineEvent。在 ThreadHandle 加 `events: broadcast::Sender<ThreadEvent>`（容量常量 THREAD_EVENT_CAP=256，对齐 deliverable TODO B2-4）。新增 `state/thread_event.rs` 定义 `ThreadEvent`（`#[derive(Debug,Clone)] #[non_exhaustive]`：TurnStarted{turn_id,started_at}/ItemAppended{turn_id,item:Box<Item>}/TurnCompleted{turn_id,status}/MetadataChanged{...}）。在现有 EngineEvent 发送的同一位点（lifecycle.rs:126/133/301, turn.rs 各 ItemAppended 点）追加一行 `handle.events.send(...)`（broadcast send 不阻塞，lagging receiver 自降级，忽略 Err）。`ThreadHandle::subscribe_events()` 暴露 receiver；`Engine` 加 `thread_events(thread_id) -> Option<Receiver>`。
被否决：把 ItemAppended 只走 thread 级、engine 级去重——会破坏现有所有订阅 EngineEvent 的 TUI/bridge，回归风险高。两层并存是 deliverable §2/§5 明确设计（event.rs 不动）。

【块3 ThreadStorage trait——抽象出可 mock 的存储接口】
现状 Storage 是具体类型，writer.rs 直接调 state.upsert_thread 等。引入 `trait ThreadStorage: Send+Sync`（在 persistence/mod.rs 或新 storage_trait.rs），方法集对齐现有调用：`upsert_thread/record_turn_start/append_item/record_turn_end/list_threads/get_thread/get_turn_items` + 新增 `load_items_page(turn_id,offset,limit)`（lazy load 入口）。为现有 `StateDb` impl 该 trait（直接转调现有方法）。writer.rs 的 `WriterState.storage: Arc<Storage>` 改为持 trait object 或保留具体类型但在 trait 后做 test mock。最小侵入版：trait 只用于测试 mock + lazy-load 读路径，生产仍走 Storage。
被否决：把整个 Storage 改成 trait object 注入——侵入面大，且 Storage::rollout_path/4 库聚合不易抽象；本阶段只抽 StateDb 的读写接口足够支撑 mock 与 lazy-load。

【块4 session_index.jsonl + 多文件 rebuild】
新增 `persistence/session_index.rs`：`SessionIndexEntry{thread_id:String, name:String, updated_at:i64}`（`Serialize/Deserialize`，serde camelCase 对齐 wire），`append_entry(base_dir, &entry)`（tokio::fs append+create，append-only，flush）、`find_name_by_id(base_dir, thread_id)`（倒序扫，最新胜，全 async 行读）、`list_latest(base_dir)`（去重取每 id 最新）。Storage 加 `session_index_path()` + `append_session_index(thread_id,name)`。rename/创建 thread 时调用。
rebuild：新增 `rebuild_indexes_from_jsonl(rollouts_dir, state:&StateDb) -> Result<RebuildStats>`（writer.rs 或新 rebuild.rs）：read_dir 遍历 *.jsonl（跳过 session_index.jsonl），对每文件调现有 `rebuild_state_from_rollout`，累计 RebuildStats{threads_rebuilt,entries_replayed}。复用现有单文件逻辑，仅加目录遍历层（B3 §7.3 伪码的多文件外壳）。
被否决：把 turn_start/turn_end entry 写进 JSONL 做完整 event-sourcing——现有 RolloutEntry 只有 Session/Item/Leaf，rebuild 已用 Item 推 turn（writer.rs:544-556），本阶段不扩 RolloutEntry case（避免 schema break），rebuild 沿用 best-effort。

## files

- `crates/zhive-core/src/state/turn_buffer.rs` — 新增。pub struct TurnHistoryBuffer{active:Option<Turn>, completed:VecDeque<Turn>, in_memory_turn_cap:usize, lazy_unloaded_count:usize} + const IN_MEMORY_TURN_CAP=50。impl: new/with_cap、start_turn(Turn)、push_item(Item)（推 active.items）、finish_turn(TurnStatus,completed_at:i64,duration_ms:Option<i64>)、enforce_cap()（清最老 turn items + 置 items_view=Summary→NotLoaded + lazy_unloaded_count+=1）、item_count()、recent_turns(offset,limit)->Vec<Turn>、all_item_ids()。doc+doctest。
- `crates/zhive-core/src/state/thread_event.rs` — 新增。pub enum ThreadEvent #[derive(Debug,Clone)] #[non_exhaustive]: TurnStarted{turn_id:TurnId,started_at:i64}/ItemAppended{turn_id:TurnId,item:Box<Item>}/TurnCompleted{turn_id:TurnId,status:TurnStatus}/MetadataChanged{name:Option<String>,status:Option<ThreadStatus>,updated_at:i64}。doc+doctest。const THREAD_EVENT_CAP=256。
- `crates/zhive-core/src/state/thread.rs` — ThreadHandle 删 items_tail/items_tail_capacity/DEFAULT_TAIL_CAPACITY；加 history:Arc<Mutex<TurnHistoryBuffer>> 与 events:broadcast::Sender<ThreadEvent>。new_idle/with_capacity/new_child 同步更新构造。push_item 改走 history.lock().push_item；item_ids 走 history.all_item_ids；新增 subscribe_events()->broadcast::Receiver<ThreadEvent>、start_turn_buffer/finish_turn_buffer 转发。ThreadStore 不变。改 push_item_respects_capacity 测试为 turn 维度断言。
- `crates/zhive-core/src/state.rs` — pub mod turn_buffer; pub mod thread_event; 重导出 TurnHistoryBuffer/ThreadEvent。更新 B2 doc 注释（已落地）。
- `crates/zhive-core/src/engine/lifecycle.rs` — start_turn：push_item 循环前调 history.start_turn(Turn::new)；ItemAppended EngineEvent 旁追加 handle.events.send(ThreadEvent::ItemAppended)；TurnStarted EngineEvent 旁追加 ThreadEvent::TurnStarted。finish_turn/cancel/fail（lifecycle.rs:296-428）：调 history.finish_turn + 追加 ThreadEvent::TurnCompleted。
- `crates/zhive-core/src/engine/turn.rs` — 各 push_item 调用点（210/306/379/425/449/622/649）后追加 handle.events.send(ThreadEvent::ItemAppended)；turn.rs:683 items_tail.read().len() 改 history item_count()；prompt 历史读点改走 history 遍历 active+completed turns 的 items。
- `crates/zhive-core/src/engine.rs` — Engine 加 thread_events(&self, thread_id:&ThreadId)->Option<broadcast::Receiver<ThreadEvent>>（经 threads().get 转 subscribe_events）。EngineConfig/spawn 不变。doc+example。
- `crates/zhive-core/src/persistence/session_index.rs` — 新增。SessionIndexEntry{thread_id:String,name:String,updated_at:i64} serde camelCase。const SESSION_INDEX_FILE="session_index.jsonl"。append_entry(dir,&entry)（tokio::fs OpenOptions append+create+flush）、find_name_by_id(dir,id)->Option<String>（async 行读倒序最新胜）、list_latest(dir)->Vec<SessionIndexEntry>。doc+doctest（no_run）。
- `crates/zhive-core/src/persistence/mod.rs` — pub mod session_index; 重导出 SessionIndexEntry。Storage 加 session_index_path()->PathBuf 与 async append_session_index(thread_id,name)->StorageResult<()>、find_thread_name(thread_id)。
- `crates/zhive-core/src/persistence/storage_trait.rs` — 新增（或并入 mod.rs）。trait ThreadStorage: Send+Sync（async_trait 或 RPITIT）含 upsert_thread/record_turn_start/append_item/record_turn_end/list_threads/get_thread/get_turn_items/load_items_page。impl ThreadStorage for StateDb 直接转调现有方法；load_items_page 用 LIMIT/OFFSET 新 SQL。doc+doctest。
- `crates/zhive-core/src/persistence/writer.rs` — 新增 pub async fn rebuild_indexes_from_jsonl(rollouts_dir:&Path, state:&StateDb)->StorageResult<RebuildStats>（read_dir 遍历 *.jsonl 跳过 session_index.jsonl，逐文件复用 rebuild_state_from_rollout，累计 RebuildStats{threads_rebuilt:u64,entries_replayed:u64}）。新增 pub struct RebuildStats #[derive(Debug,Default)]。doc+doctest no_run。
- `crates/zhive-core/src/persistence/state_db.rs` — 新增 load_items_page(&self,turn_id:&TurnId,offset:i64,limit:i64)->StorageResult<Vec<Item>>（SELECT payload ... ORDER BY seq LIMIT ?2 OFFSET ?3）。供 ThreadStorage trait 与 lazy-load 读路径。doc+doctest no_run。

## newTypes

- pub struct TurnHistoryBuffer { active: Option<Turn>, completed: std::collections::VecDeque<Turn>, in_memory_turn_cap: usize, lazy_unloaded_count: usize }
- pub const IN_MEMORY_TURN_CAP: usize = 50;
- impl TurnHistoryBuffer: pub fn new()->Self; pub fn start_turn(&mut self, turn: Turn); pub fn push_item(&mut self, item: zhive_proto::domain::Item); pub fn finish_turn(&mut self, status: TurnStatus, completed_at: i64, duration_ms: Option<i64>); fn enforce_cap(&mut self); pub fn item_count(&self)->usize; pub fn recent_turns(&self, offset: usize, limit: usize)->Vec<Turn>; pub fn all_item_ids(&self)->Vec<zhive_proto::domain::ItemId>
- #[derive(Debug, Clone)] #[non_exhaustive] pub enum ThreadEvent { TurnStarted{turn_id:TurnId,started_at:i64}, ItemAppended{turn_id:TurnId,item:Box<Item>}, TurnCompleted{turn_id:TurnId,status:TurnStatus}, MetadataChanged{name:Option<String>,status:Option<ThreadStatus>,updated_at:i64} }
- pub const THREAD_EVENT_CAP: usize = 256;
- ThreadHandle 字段变更: history: Arc<tokio::sync::Mutex<TurnHistoryBuffer>>, events: tokio::sync::broadcast::Sender<ThreadEvent>（取代 items_tail/items_tail_capacity）
- impl ThreadHandle: pub fn subscribe_events(&self)->broadcast::Receiver<ThreadEvent>
- impl Engine: pub fn thread_events(&self, thread_id: &ThreadId)->Option<broadcast::Receiver<ThreadEvent>>
- pub trait ThreadStorage: Send + Sync { async fn upsert_thread(&self,&Thread)->StorageResult<()>; async fn record_turn_start(&self,&ThreadId,&TurnId,i64)->StorageResult<()>; async fn append_item(&self,&TurnId,i64,&Item)->StorageResult<()>; async fn record_turn_end(&self,&TurnId,TurnStatus,Option<&TurnError>,i64,Option<i64>)->StorageResult<()>; async fn list_threads(&self)->StorageResult<Vec<Thread>>; async fn get_thread(&self,&ThreadId)->StorageResult<Option<Thread>>; async fn get_turn_items(&self,&TurnId)->StorageResult<Vec<Item>>; async fn load_items_page(&self,&TurnId,i64,i64)->StorageResult<Vec<Item>> }
- impl StateDb: pub async fn load_items_page(&self, turn_id:&TurnId, offset:i64, limit:i64)->StorageResult<Vec<Item>>
- #[derive(Debug, Serialize, Deserialize, PartialEq)] #[serde(rename_all="camelCase")] pub struct SessionIndexEntry { thread_id: String, name: String, updated_at: i64 }
- session_index: pub async fn append_entry(base_dir:&Path,&SessionIndexEntry)->StorageResult<()>; pub async fn find_name_by_id(base_dir:&Path,thread_id:&str)->StorageResult<Option<String>>; pub async fn list_latest(base_dir:&Path)->StorageResult<Vec<SessionIndexEntry>>
- #[derive(Debug, Default)] pub struct RebuildStats { pub threads_rebuilt: u64, pub entries_replayed: u64 }
- pub async fn rebuild_indexes_from_jsonl(rollouts_dir:&Path, state:&StateDb)->StorageResult<RebuildStats>

## redlineImpact
不触红线，但有两点需注意：
1. 禁新 crate：全部用已在依赖（tokio broadcast/mpsc/Mutex、tokio_util、sqlx、serde、serde_json、thiserror、tracing）。ThreadStorage trait 若用 async fn in trait（RPITIT，Rust 2024 稳定）则无需 async_trait——但需确认现有代码 async_trait 是否已在依赖：现有 writer/state_db 未用 async_trait（直接 inherent async fn），故 ThreadStorage 优先用 RPITIT（`trait X { async fn ... }`）+ 不要求 dyn 兼容（trait 仅用于泛型 mock 与 StateDb impl，不做 trait object）。若必须 dyn-safe 才需 async_trait——查 Cargo 是否已含 async_trait；deliverable B2 草图用了 async_trait 但现状代码没用，建议本阶段用 RPITIT 避免引依赖。**redline 标注：确认 async_trait 不在依赖清单则禁止引入。**
2. 禁 unwrap/expect 非测试：session_index/load_items_page/rebuild 全走 `?` + StorageError；TurnHistoryBuffer 同步方法无 fallible 点（纯内存操作）；broadcast send 的 Err 用 `let _ =` 忽略（lagging 自降级，与现有 events_tx().send 一致 inner.rs/turn.rs 既有模式）。
3. 公开 API 必须 doc+doctest：所有新 pub fn/struct/trait/enum 加 doc comment + 至少一个 doctest（StateDb 现有用 no_run 模式可照抄）。
4. unsafe：无。

## crossModuleDeps

- 与现有 EngineEvent（event.rs）：ThreadEvent 是并列第二层，绝不替换 EngineEvent；所有 EngineEvent 发送点（lifecycle.rs:126/133/301, turn.rs 7 处）保留不动，仅旁加 ThreadEvent.send。TUI/bridge 现订阅 EngineEvent 不受影响（任务#7/#8 的 server 读 handler 可二选一订阅）。
- 与现有 StorageWriteOp/PersistenceWriter（writer.rs）：不改 write-through 主链路（引擎 enqueue_storage_op → writer drain → JSONL+StateDb）。TurnHistoryBuffer 不持 write_queue（草图的 write_queue 字段被现有 PersistenceWriter 取代——避免双 writer），内存 buffer 与持久化解耦，引擎在同一位点既调 history 又调 enqueue_storage_op（lifecycle/turn 已有 enqueue 调用，新增 history 调用与之并列）。
- 与 server 层（任务#7 缺口）：本任务提供 thread_events()/recent_turns()/load_items_page() 作为读 API 基座，但不实现 thread/read、turn/get_items handler（那属 server 缺口，由任务#7 接线）。lazy-load 三态对外语义由 server handler 据 TurnItemsView 决定是否回填。
- 与 fork（任务#8 client-native）：Leaf 指针仍写 target_id:None（writer.rs:384 不动），SessionIndexEntry 的 parentSession/fork-tree 留给 #8；session_index 只做 thread_id↔name。
- 与 compaction（engine/compaction.rs）：enforce_cap 把老 turn 降级 Summary 与 compaction 的 ContextCompaction item 是两套机制（前者内存窗口、后者语义压缩），互不冲突；TurnHistoryBuffer.lazy_unloaded_count 仅反映内存驱逐不反映 compaction。
- 与 EngineConfig.storage 注入（engine.rs:187）：ThreadStorage trait 不改 EngineConfig 类型（仍 Option<Arc<Storage>>），trait 仅供测试 mock 与 lazy-load 泛型读，生产路径仍具体 Storage。

## tests

- TurnHistoryBuffer: start_turn→push_item×N→finish_turn 后 active=None、completed 末元素 items 完整、status 正确（单元测试，非 async）
- TurnHistoryBuffer enforce_cap: 推 cap+3 个 turn 后，最老 3 个 turn items 被清空且 items_view=Summary/NotLoaded、lazy_unloaded_count==3、completed.len()==cap、turn 头（id/started_at/status）保留
- TurnHistoryBuffer doctest: 构造 + start/push/finish 一轮断言 item_count
- ThreadHandle: push_item 后 item_ids 含该 id（turn 维度，替换原 push_item_respects_capacity）；subscribe_events 收到 ItemAppended（tokio::test，发后 recv）
- ThreadEvent doctest: 构造一个 TurnStarted 变体断言字段
- Engine thread_events: 不存在 thread 返回 None；start_turn 后 subscribe 收到 TurnStarted+ItemAppended（tokio::test，复用现有 engine 测试夹具）
- session_index append→find_name_by_id 往返：写两条同 id 不同 name，find 返回最新（tokio::test + tempfile）
- session_index list_latest 去重：3 条覆盖 2 id 返回 2 条最新
- session_index doctest no_run: append_entry 示例
- StateDb load_items_page: 写 5 item，offset=2 limit=2 返回 seq 2,3（tokio::test 复用 open_temp）
- StateDb load_items_page doctest no_run
- rebuild_indexes_from_jsonl: tempfile 造 2 个 thread jsonl + 1 个 session_index.jsonl，rebuild 后 RebuildStats.threads_rebuilt==2、session_index 被跳过、state.db list_threads 含 2 thread（tokio::test）
- rebuild_indexes_from_jsonl doctest no_run
- ThreadStorage trait: 用 StateDb impl 经 trait 调 upsert_thread/get_thread 往返（确认 trait 转调正确）；可选 in-memory mock 验证 trait 泛型可注入
- 回归: 现有 writer.rs writer_applies_ops_and_persists_items / rebuild_state_from_rollout_round_trip / engine.rs engine_turn_with_storage_writes_rollout_and_state_db 仍绿（push_item 内部改动后端到端不变）

## risks
1. push_item 语义变更是最大回归面：现状 items_tail 是 thread 级扁平 VecDeque，prompt 构建从它读历史（inner.rs doc 描述）。改为 turn 维度后，prompt 读点必须能跨 completed turns 拉取 UserMessage/AgentMessage，否则多轮对话历史断裂。必须先定位 prompt 实际读 items_tail 的代码（inner.rs doc 提到但需在 prompt.rs/turn.rs 确认精确行）并同步改造——这是 must-do 否则 LLM 丢上下文。

2. TurnHistoryBuffer.active 与 ActiveTurn 双份 turn 态（deliverable TODO B2-6）：靠 turn_id 同步，需在 finish_turn 处 debug_assert turn_id 一致，避免分裂。

3. 锁顺序（TODO B2-3）：固定 status.write → history.lock → active_turn.lock；当前 lifecycle.rs start_turn 是先 active_turn.lock 再 status.write（lifecycle.rs:104-118），引入 history.lock 后需确认不与 active_turn 锁交叉死锁——history 与 active_turn 在 start_turn 内分别短锁不重叠即可。

4. RPITIT vs async_trait：若 ThreadStorage 需 dyn-safe（writer 持 trait object）则 RPITIT 不够，须引 async_trait——但那触红线。规避：trait 仅做泛型/mock 不做 dyn，生产 writer 仍持具体 Storage。务必先确认依赖清单无 async_trait。

5. enforce_cap 降级策略：Summary vs NotLoaded 二级降级若实现过度复杂可先只做一级（直接 NotLoaded + 清 items），Summary 态留给 compaction 填充——避免本阶段引入 summary 生成逻辑。

6. ThreadEvent 广播在每个 item 点新增一次 send：高频路径多一次 clone（Box<Item>），但与现有 EngineEvent.send 同量级，broadcast 无背压风险（lagging 自降级）。

## recommendation
实现顺序（每步可独立 cargo check -p zhive-core --lib 验证）：
1. 先 session_index.rs + StateDb::load_items_page + rebuild_indexes_from_jsonl + RebuildStats（块4+块3 的 SQL 部分）——纯增量、零回归、不碰 ThreadHandle，最先落地降风险。
2. 再 ThreadStorage trait + impl for StateDb（块3）——仍纯增量。
3. 再 turn_buffer.rs（块1 的 TurnHistoryBuffer 独立类型 + 单元测试）——独立可测，不接线。
4. 再 thread_event.rs（块2 类型）——独立。
5. 最后接线（高风险）：ThreadHandle 换 items_tail→history+events，改 push_item/item_ids，同步改 lifecycle.rs/turn.rs/prompt 读点，加 Engine::thread_events。这步必须连带把 prompt 历史读路径改对，建议先 grep 定位 prompt 实际读 items_tail 的精确行（本调研只读未深入 prompt.rs）再动。

范围建议（本阶段做到什么程度）：
- 做：TurnHistoryBuffer 三态 + enforce_cap（建议先只一级 NotLoaded 降级）、ThreadEvent 两层 broadcast、ThreadStorage trait（仅 mock/泛型用途）、session_index、多文件 rebuild、load_items_page。
- 推迟：fork/leaf-tree（→任务#8）、Summary 态自动生成（→与 compaction 合并设计）、server 层 thread/read/turn/get_items handler 接线（→任务#7）、按 token 而非 turn 数 cap（TODO B2-1，YAGNI）、FlushBarrier{oneshot ack}（现有 Flush{thread_id} 已够，加 ack 等真需要 reverse_rpc barrier 时再补）。
- 不做：把 Storage 整体 trait 化、改 StorageWriteOp 命名对齐草图、扩 RolloutEntry 加 turn_start/turn_end case（rebuild 已 best-effort）。

务必：第5步前先确认 async_trait 不在依赖（决定 trait 用 RPITIT）；先定位并改对 prompt 历史读点；保持 EngineEvent 完全不动。
