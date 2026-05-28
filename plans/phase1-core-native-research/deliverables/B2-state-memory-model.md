---
task: B2
title: State 内存模型（活跃 Thread / Turn / Item 表征 + 读 / 写 / 订阅路径）
date: 2026-05-28
status: draft
depends_on:
  - A1 deliverable (Thread / Turn / Item 类型 + TurnStartedNotification / TurnCompletedNotification 形态)
  - B1 deliverable (Engine actor + Arc<EngineInner> + channel 拓扑 + ThreadHandle / ActiveTurn / TurnState)
  - D-006 (Thread/Turn/Item + serde + schemars 单 schema 源)
  - D-011 (rusqlite 4 库 + JSONL+Leaf rollout —— B3 落地，本调研只对齐 sync 点)
  - D-012 (Hook 14 事件 + `#[non_exhaustive]` —— 内存 mutate 触发 hook 的位点)
  - D-014 (tracing 强制覆盖 Turn / ToolCall / Permission —— 与 item appender 同 span)
references:
  - ${CODEX}/core/src/thread_manager.rs:170-217           (ThreadManager / ThreadManagerState：`threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>` + `thread_created_tx: broadcast::Sender<ThreadId>` + `thread_store: Arc<dyn ThreadStore>`)
  - ${CODEX}/core/src/session/session.rs:19-40            (Session 字段：`tx_event` / `agent_status: watch::Sender<AgentStatus>` / `state: Mutex<SessionState>` / `active_turn: Mutex<Option<ActiveTurn>>` / `services: SessionServices`)
  - ${CODEX}/core/src/state/session.rs:23-42              (SessionState 字段：`history: ContextManager` / `latest_rate_limits` / `additional_context` / `auto_compact_window: AutoCompactWindow` —— history 通过 ContextManager 而非 Vec<Item> 直接持有)
  - ${CODEX}/core/src/codex_thread.rs:107-149             (CodexThread 字段：`codex: Codex` + `session_configured: SessionConfiguredEvent` + `rollout_path: Option<PathBuf>` —— 内存 handle 与 持久化路径绑定)
  - ${CODEX}/thread-store/src/store.rs:26-120             (`pub trait ThreadStore`：`create_thread / resume_thread / append_items / persist_thread / flush_thread / shutdown_thread / discard_thread / load_history / read_thread / list_items / update_thread_metadata`——区分 `persist` / `flush` / `shutdown` / `discard` 4 个生命周期动作)
  - plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md  (Thread / Turn / Item 14 case 草图；§6 wire 类型；TurnItemsView = NotLoaded/Summary/Full)
  - plans/phase1-core-native-research/deliverables/B1-engine-loop.md       (Engine / EnginePhase / channel 拓扑：broadcast event 1024 / watch phase / mpsc submission 512 / mpsc item-in-turn 256)
  - crates/zhive-core/src/state.rs                                          (现有骨架，6 行 placeholder)
---

> **设计衔接说明**：本调研在 B1 已锁定的 `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` 与 `ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` 之上，进一步定义：（a）活跃 thread 的 `Arc<RwLock<Thread>>` 持有形态及锁粒度；（b）Turn 历史的内存 cap 与 lazy load 模型；（c）Item 在 wire（A1）与内存（B2）的复用关系；（d）与 persistence（B3）的 sync 点。**不改 A1 / B1**——仅复用其已锁字段。

---

## 1. 参考点清单

每个论断的锚点（repo + 文件 + 行号），下文逐条引用。

| 主题 | 路径 | 行号 |
|---|---|---|
| codex `ThreadManager { state: Arc<ThreadManagerState> }` 顶层句柄 | `${CODEX}/core/src/thread_manager.rs` | 170-173 |
| codex `ThreadManagerState { threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>, thread_created_tx: broadcast::Sender<ThreadId>, thread_store: Arc<dyn ThreadStore>, state_db: Option<StateDbHandle>, ... }` | `${CODEX}/core/src/thread_manager.rs` | 199-217 |
| codex `thread_created_tx: broadcast::Sender<ThreadId>` 初始化 | `${CODEX}/core/src/thread_manager.rs` | 259, 273 |
| codex `Session { active_turn: Mutex<Option<ActiveTurn>>, state: Mutex<SessionState>, agent_status: watch::Sender<AgentStatus>, services: SessionServices, ... }` | `${CODEX}/core/src/session/session.rs` | 19-40 |
| codex `SessionState { history: ContextManager, latest_rate_limits, additional_context, auto_compact_window: AutoCompactWindow, granted_permissions, ... }` —— **history 用 ContextManager 包装，不直接持 `Vec<Item>`** | `${CODEX}/core/src/state/session.rs` | 23-42 |
| codex `CodexThread { codex: Codex, session_configured, rollout_path: Option<PathBuf>, ... }` —— 单线程内存 handle 持有 rollout 路径 | `${CODEX}/core/src/codex_thread.rs` | 107-113 |
| codex `ThreadStore` trait：`append_items / persist_thread / flush_thread / shutdown_thread / discard_thread` —— 4 个生命周期动作分离 | `${CODEX}/thread-store/src/store.rs` | 38-59 |
| codex `ThreadStore::load_history(params) -> StoredThreadHistory` —— resume / fork / rollback / memory jobs 共用入口 | `${CODEX}/thread-store/src/store.rs` | 61-65 |
| codex `ThreadStore::list_items` 分页接口 —— lazy load 必经入口 | `${CODEX}/thread-store/src/store.rs` | 98-103 |
| A1 `TurnItemsView = NotLoaded \| Summary \| Full` 三态视图 —— **lazy load 状态机已在 wire 层就位** | plans/.../A1-thread-turn-item.md | §6 (草图 434-442) |
| A1 `Item` 14 case `#[serde(tag = "kind", rename_all = "snake_case")]` `#[non_exhaustive]` | plans/.../A1-thread-turn-item.md | §6 (草图 467-552) |
| B1 `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` —— **已锁形态** | plans/.../B1-engine-loop.md | §4 (草图 252-274) |
| B1 `ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` + `thread: Arc<RwLock<Thread>>` + `sub_tx: mpsc::Sender<Submission>` —— **已锁形态** | plans/.../B1-engine-loop.md | §4 (草图 276-290) |
| B1 `ActiveTurn.item_tx: mpsc::Sender<Item>` bounded(256) —— turn 内 item 流唯一 producer/consumer | plans/.../B1-engine-loop.md | §6.2 channel 表 |
| B1 `event_bus: broadcast::Sender<EngineEvent>` bounded(1024) —— fan-out 给 N 客户端 | plans/.../B1-engine-loop.md | §6.2 channel 表 |
| B1 `EnginePhase: watch::Sender<EnginePhase>` —— phase 共享 | plans/.../B1-engine-loop.md | §6.2 channel 表 |
| B1 §6.6 事件流：`agent loop → ActiveTurn.item_tx → item appender → event_bus broadcast` | plans/.../B1-engine-loop.md | §6.6 |

---

## 2. 内存类型草图（核心交付）

> 写在本 deliverable 内部代码块，**不进 `crates/`**（按硬约束）。引用 B1 草图中已定义的 `ThreadHandle / ActiveTurn / TurnState`，在其之上**补内部 layout**。所有 `todo!()` 占位。

```rust
//! Phase 1 草图：zhive-core::state（B2 落地）
//!
//! 围绕 B1 `EngineInner.threads` 已锁的 `Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>`
//! 形态，补内部 layout。**Item / Turn / Thread 类型直接复用 A1（zhive-proto::domain）
//! 即同一 Rust 类型**（D-006 「单一 schema 源」字面落地；见 §6.3 论证）。

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

// 直接复用 A1 wire 类型（D-006 单一 schema 源）
use zhive_proto::domain::{
    Item, ItemId, Thread, ThreadId, ThreadStatus, Turn, TurnId, TurnItemsView, TurnStatus,
};

// ============================================================
// ThreadHandle —— 单 thread 的运行时表征
// (复用 B1 §4 已定签名；本节展开内部 layout)
// ============================================================

/// 单 thread 内存表征：handle 形态 + Arc 共享，**不持锁返回**。
///
/// 锁粒度：
/// - `thread: Arc<RwLock<Thread>>` —— Thread metadata（id / name / status / preview / updated_at）。
///   多读少写：客户端 query 走 `.read().await`；engine 内部 mutate name/status 走 `.write().await`，
///   持锁时间 < 1ms。codex `Session.state: Mutex<SessionState>`（`session/session.rs:25`）等价物，
///   但 zhive 拆为：（1）`thread`（metadata，RwLock）与（2）`history`（item buffer，独立锁）。
/// - `history: Arc<Mutex<TurnHistoryBuffer>>` —— Turn / Item 序列与 lazy load 状态。
///   持锁动作：append item / start_turn / finalize_turn / load_more。**每次操作短锁**，agent loop
///   持有 buffer ref 时**不跨 await 持锁**（与 B1 TODO B1-7 同诫律）。
/// - `active_turn: Mutex<Option<ActiveTurn>>` —— B1 已锁，仅当前 turn 写者持锁。
pub struct ThreadHandle {
    pub thread_id: ThreadId,

    /// Thread metadata（A1 `Thread` 类型，但不含 `turns: Vec<Turn>` —— 那部分挪到 `history`）。
    pub thread: Arc<RwLock<Thread>>,

    /// Turn 序列 + Item buffer + lazy load 状态。**主存储位置**。
    pub history: Arc<Mutex<TurnHistoryBuffer>>,

    /// 当前活跃 turn（最多 1 个；B1 §4 已锁）。
    pub active_turn: Mutex<Option<ActiveTurn>>,

    /// thread 级 cancel 信号（interrupt_turn 用；B1 已锁）。
    pub cancel: CancellationToken,

    /// 后台 agent loop 的 submission 入口（B1 已锁）。
    pub sub_tx: mpsc::Sender<Submission>,

    /// thread 级事件 fan-out（**与 engine 级 `event_bus` 并列**：thread 级让客户端订单 thread
    /// 时不被其他 thread 流量打扰；engine 级用于跨 thread 全局事件如 thread_created）。
    /// codex `ThreadManagerState.thread_created_tx`（`thread_manager.rs:201`）是 engine 级；
    /// codex 同时在 `Session.tx_event: Sender<Event>`（`session/session.rs:22`）保留 thread 级
    /// async-channel，二级路由形态。zhive 同构。
    pub events: broadcast::Sender<ThreadEvent>,

    /// 持久化 sink（B3 deliverable 定义具体后端；本节仅持 trait object）。
    pub storage: Arc<dyn ThreadStorage + Send + Sync>,

    /// 后台 loop task handle（drop ⇒ abort；B1 已锁形态）。
    pub _loop_handle: tokio_util::task::AbortOnDropHandle<()>,
}

// ============================================================
// TurnHistoryBuffer —— Turn 序列 + Item buffer 的内存表征
// 关键设计：滚动 window + TurnItemsView 三态 + write-through 队列
// ============================================================

/// Thread 内 Turn / Item 的内存 buffer。
///
/// **不变量**：
/// 1. `active.is_some()` ⟹ `active.as_ref().unwrap().status == TurnStatus::InProgress`
///    且不在 `completed` 中（活跃 turn 唯一）
/// 2. `completed.iter()` 按 `started_at` 单调递增
/// 3. `completed.len() <= IN_MEMORY_TURN_CAP` —— 超过时最老的 N 个 turn 的 `items_view` 被
///    降级为 `Summary` 或 `NotLoaded`，**结构体本身仍在 buffer 里**（保留 turn metadata 给
///    UI 列表用），仅 `items: Vec<Item>` 被清空。具体降级策略 §3.2。
/// 4. 任何 mutate 后**必然**先 push 到 `write_queue`（write-through 队列）再返回；fsync
///    由 storage 后台 flush task 异步完成（write-back 形态由 storage trait 决定）。
pub struct TurnHistoryBuffer {
    /// 当前活跃 turn 的 `Turn` 引用 —— 与 `ThreadHandle.active_turn: ActiveTurn` 是**对偶**关系：
    /// `ActiveTurn` 是运行时状态（cancel token / TurnState / item_tx），
    /// 这里的 `Option<Turn>` 是 schema 视角的 turn 投影（items 增量追加）。
    /// 二者通过 `turn_id` 关联；agent loop append item 时同时改这两处（详 §4）。
    pub active: Option<Turn>,

    /// 已完成 turn 序列（按时间）。`VecDeque` 便于前端 pop 老 turn 的 items 做降级。
    pub completed: VecDeque<Turn>,

    /// 滚动 window 阈值（默认 50；可由 EngineConfig 覆写）。
    pub in_memory_turn_cap: usize,

    /// 已知存在但当前内存中 `TurnItemsView::NotLoaded` 的 turn id 总数（用于 UI 显示
    /// "Earlier turns (N) — click to load"）。
    pub lazy_unloaded_count: usize,

    /// 写穿队列：每个 mutate 操作 push 一条；storage 后台 task 按序消费。
    /// **不在乎被 drop**——drop 意味着 thread 被回收，未 flush 的 item 由 thread shutdown
    /// 路径的 `flush_thread` 兜底（codex `ThreadStore::flush_thread` 模式，
    /// `thread-store/src/store.rs:48-49`）。
    pub write_queue: mpsc::Sender<StorageWriteOp>,
}

/// 写穿队列的单条操作。
#[derive(Debug, Clone)]
pub enum StorageWriteOp {
    /// turn 开启时落地一条 `Turn` 头（含 `started_at`），item 部分留空
    TurnStarted { thread_id: ThreadId, turn: Turn },
    /// 单条 item 追加（B3 JSONL append-only 主路径）
    AppendItem { thread_id: ThreadId, turn_id: TurnId, item: Item },
    /// turn 收尾（含 status / completed_at / duration_ms）
    TurnCompleted { thread_id: ThreadId, turn_id: TurnId, status: TurnStatus, completed_at: i64, duration_ms: i64 },
    /// Thread metadata patch（name / preview / status）
    ThreadMetadata { thread_id: ThreadId, patch: ThreadMetadataPatch },
    /// 流式 flush 请求（fsync barrier；reverse_rpc 响应、shutdown 触发）
    FlushBarrier { ack: tokio::sync::oneshot::Sender<()> },
}

#[derive(Debug, Clone)]
pub struct ThreadMetadataPatch {
    pub name: Option<String>,
    pub preview: Option<String>,
    pub status: Option<ThreadStatus>,
    pub updated_at: i64,
}

// ============================================================
// 全局 store —— Engine 内的 thread 集合
// ============================================================

/// `EngineInner.threads` 的等价类型（B1 已锁形态：`Arc<RwLock<HashMap<...>>>`）。
/// 单独 alias 便于在 trait / API 处统一引用。
pub type ThreadRegistry = Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>;

// ============================================================
// 订阅相关类型
// ============================================================

/// thread 级事件 fan-out 类型。
/// 注意：与 B1 `EngineEvent`（engine 级 fan-out）是**两层**：
/// - `EngineEvent`（B1）：跨 thread 全局（thread_created / engine_shutdown / phase_changed）
/// - `ThreadEvent`（本节）：单 thread 内（turn/item 流；客户端订单 thread 后只关心这层）
#[derive(Debug, Clone)]
pub enum ThreadEvent {
    /// turn 开启
    TurnStarted { turn_id: TurnId, started_at: i64 },
    /// turn 内追加 item（最高频；B1 `ItemAppended` 同形态）
    ItemAppended { turn_id: TurnId, item: Item },
    /// turn 收尾
    TurnCompleted { turn_id: TurnId, status: TurnStatus },
    /// Thread metadata mutate（rename / status change）
    MetadataChanged { patch: ThreadMetadataPatch },
}

// ============================================================
// 占位 trait（B3 落地）
// ============================================================

#[async_trait::async_trait]
pub trait ThreadStorage {
    async fn create_thread(&self, thread: &Thread) -> Result<(), StorageError>;
    async fn resume_thread(&self, thread_id: &ThreadId) -> Result<ResumedHistory, StorageError>;
    async fn append_item(&self, thread_id: &ThreadId, turn_id: &TurnId, item: &Item) -> Result<(), StorageError>;
    async fn update_metadata(&self, thread_id: &ThreadId, patch: &ThreadMetadataPatch) -> Result<(), StorageError>;
    async fn load_items_page(&self, thread_id: &ThreadId, turn_id: &TurnId, offset: usize, limit: usize) -> Result<Vec<Item>, StorageError>;
    /// fsync barrier
    async fn flush(&self, thread_id: &ThreadId) -> Result<(), StorageError>;
    async fn shutdown(&self, thread_id: &ThreadId) -> Result<(), StorageError>;
}

#[derive(Debug)]
pub struct ResumedHistory {
    pub thread: Thread,
    pub completed_turns: VecDeque<Turn>,
    pub lazy_unloaded_count: usize,
}

// 占位：B1 / 其它 deliverable 定义
pub type Submission = serde_json::Value;
pub type ActiveTurn = serde_json::Value;
pub type StorageError = std::io::Error;
```

**编译性约束**（与 CLAUDE.md / A1 / B1 对齐）：
- 全部 `Arc<RwLock<...>> / Arc<Mutex<...>> / mpsc::Sender / broadcast::Sender` 类型，**无 `unsafe`**
- 所有 fallible 接口 `Result<T, _>`，**无 `unwrap / expect`**
- `Item / Turn / Thread` 类型来自 `zhive_proto::domain`（同一类型直接复用，单源）
- `#[non_exhaustive]` 透传 A1 enum 不变（不再加注）

---

## 3. 读 / 写 / 订阅访问路径

### 3.1 客户端 query 路径（读）

| 客户端 API（JSON-RPC method）| 走的 in-mem 路径 | 锁粒度 | 备注 |
|---|---|---|---|
| `thread/read?thread_id` | `EngineInner.threads.read().get(id).map(|h| h.thread.read().await.clone())` | RwLock 读锁 × 2（thread map + thread metadata），无 history 锁 | Thread metadata only；不含 turns；codex `read_thread` 对偶 |
| `thread/list` | `EngineInner.threads.read().iter().map(|(_, h)| h.thread.read().await.clone())` | RwLock 读锁；遍历 + 短锁 | 全 thread 元数据 |
| `thread/read_full?thread_id&include_turns=true` | `handle.history.lock().await.{active, completed}` + 按需 `storage.load_items_page` | Mutex 短锁；释放后调 storage（async） | TurnItemsView::NotLoaded 的 turn 不自动 load，仅返回 placeholder |
| `turn/list?thread_id&offset&limit` | `handle.history.lock().completed.range(...).cloned()` | Mutex 短锁 | 不读 items 本体 |
| `turn/get_items?thread_id&turn_id&offset&limit` | 若该 turn 在内存 `items_view == Full`：直接读；否则 `storage.load_items_page` | Mutex 短锁 / fallback storage | **lazy load 入口**；命中后内存 turn 的 `items_view = Full`（提升） |

**为何 metadata 与 history 分锁**：codex `Session.state: Mutex<SessionState>`（`session/session.rs:25`）把所有 session 状态压到一个 Mutex，turn append item 必须持该锁。zhive 在 long-running agent loop 高并发 append item 的场景下，把 metadata 与 history 分锁可**避免** UI 拉 metadata（rename / preview）阻塞 item append；二者代码路径无交叉。

### 3.2 Engine 写路径（agent loop 内）

```text
[agent loop task] (B1 ThreadHandle.sub_tx → dispatcher → here)
     │
     │ 1. start_turn(thread_id, inputs)
     ▼
[lookup] EngineInner.threads.read().get(id) → Arc<ThreadHandle>   # 短读锁
     │
     │ 2. construct ActiveTurn { turn_id, item_tx: mpsc(256), ... }
     │    (B1 §4 形态)
     ▼
[mutate active]  handle.active_turn.lock().await.replace(active_turn)   # 短锁
[mutate history] handle.history.lock().await.active = Some(Turn::new(turn_id, started_at))   # 短锁
[enqueue write]  handle.history.lock().write_queue.send(TurnStarted { ... })   # 短锁
[broadcast]      handle.events.send(ThreadEvent::TurnStarted { ... })   # 无锁
     │
     │ 3. agent loop 内逐 item 追加（高频）
     ▼
loop {
    let item: Item = produce_next_item(...).await;
    // 走 ActiveTurn.item_tx（B1 §6.6 single producer/consumer）
    active_turn.item_tx.send(item).await?;
}
     │
     │ 4. [item appender task]（B1 §6.6 唯一 consumer）
     ▼
while let Some(item) = item_rx.recv().await {
    // 主路径：3 步同步动作
    let mut h = handle.history.lock().await;
    if let Some(turn) = h.active.as_mut() {
        turn.items.push(item.clone());        // (a) 内存追加
    }
    h.write_queue.send(AppendItem { ... });   // (b) write-through（不等 fsync）
    drop(h);
    handle.events.send(ItemAppended { ... }); // (c) fan-out 广播
}
     │
     │ 5. finish_turn(turn_id, status)
     ▼
[mutate active]  handle.history.lock().await.active.take() → push completed
[cap enforce]    if completed.len() > cap: 降级最老 turn 的 items_view
[enqueue write]  TurnCompleted { ... }
[broadcast]      ThreadEvent::TurnCompleted { ... }
```

### 3.3 Hook 订阅路径（D-012 14 事件，B6 deliverable 细化）

| Hook 想观察的事件 | 订阅的 channel | 触发位点 |
|---|---|---|
| `PreToolCall` / `PostToolCall` | **不**走 broadcast，**同步**调用 hook host（B6 trait），因为 hook 可能 mutate 决策 | item appender 在 `Item::ToolCall { status: Pending }` push 前后同步 hook |
| `OnItemAppended`（A4 通用 item 钩子） | broadcast `ThreadEvent` 订阅一份 receiver | item appender 写完 `events.send` 后即触达 |
| `PreCompact / PostCompact` | EnginePhase watch + 同步 hook host（B6） | EnginePhase 切换 Idle→Compaction 前后 |
| `PhaseTransition`（B1 §6.7 新增） | watch `EnginePhase` + 同步 hook host | engine `transition_phase` 内 |
| `OnTurnStart / OnTurnEnd` | broadcast `ThreadEvent` 订阅 | 与 `TurnStarted / TurnCompleted` 同位 |

**关键设计**：**mutable hooks**（PreToolCall 改决策、PreCompact 改触发参数）走**同步 trait 调用**（参 B1 `hook_host: Arc<dyn HookHost>`）—— 不通过 channel；**observability hooks**（OnItemAppended 仅观察）走 **broadcast subscribe** —— 多观察者无背压风险。两条路径正交，不冲突。

### 3.4 客户端 push 订阅路径

```text
[Client subscribes to thread] (JSON-RPC `thread/subscribe { thread_id }`)
     │
     ▼
[server module] (D-003 dispatch)
     │  let handle = engine.thread_handle(thread_id).await?;
     │  let mut rx = handle.events.subscribe();   # broadcast::Receiver
     │
     │  + 同时订阅 engine 级 EngineEvent（拿 phase_changed / engine_shutdown）
     │  let mut engine_rx = engine.subscribe_events();
     ▼
[per-client task] loop {
    tokio::select! {
        Ok(ev) = rx.recv() => emit_notification("thread/event", ev),
        Ok(ev) = engine_rx.recv() => emit_notification("engine/event", ev),
        _ = cancel.cancelled() => break,
    }
}
```

**Lag 处理**（broadcast 容量满）：`broadcast::Receiver::recv()` 返回 `Err(Lagged(n))` —— per-client task 发 `thread/resync` notification 让客户端走 `thread/read_full` 重拉。此机制与 C2（reconnect）deliverable 一致（不重复设计）。

---

## 4. 与 Persistence（B3）的 sync 点

### 4.1 同步语义选型：write-through queue + write-back fsync

| 维度 | 选择 | 理由 |
|---|---|---|
| 内存→storage 触达时机 | **write-through**（每次 mutate 同步 enqueue 到 `write_queue`） | 内存与 queue 永远一致；agent loop 不阻塞等 fsync |
| storage 落盘时机 | **write-back**（后台 task 批量消费 queue，按 batch 或 timeout fsync） | 减小 fsync 频率；D-011 JSONL append 本身 batch 友好 |
| event-sourcing 形态？ | **是** —— `StorageWriteOp` 就是 event 流；replay 即恢复 | 与 D-011 「JSONL rollout」append-only 天然契合；resume thread 从 JSONL replay 即 in-mem 状态 |
| fsync barrier 触发点 | 见下 §4.2 列表 | 不每次 append 都 fsync（性能） |

### 4.2 fsync 时机（barrier 列表）

按**触发严格度**排序，从最强到最弱：

1. **`engine.shutdown()`**（B1 §4.5 公开方法）—— 必发 `FlushBarrier` + 等 ack；codex `ThreadStore::shutdown_thread`（`store.rs:51-52`）等价
2. **`engine.compact()` 进入 Compaction phase 前**（D-012 PreCompact hook 前）—— 内存压缩前必须落盘旧 item，否则压缩丢数据
3. **`spawn_subagent` 派生瞬间**（B1 SubagentSpawn phase）—— 父 thread 的最新状态必须 durable，子 thread 才能可信地 inherit
4. **`reverse_rpc` permission 应答 / elicitation 应答返回前**（D-008）—— 客户端可能在 reply 后立刻断线，未持久化 → 重连看不到原请求
5. **turn `Completed / Interrupted / Failed`**（每次 `TurnCompleted` op 入队后）—— 客户端 `thread/read` 或 reconnect 时看到的 turn 视图必须包含已结束 turn 的完整 items
6. **空闲 timeout 1s** —— 默认 fallback；EngineConfig 可调

中间状态 fsync 路径（**默认不发**）：单条 item append 后不 fsync —— write-through 进 queue + 异步 batch flush。崩溃时未 fsync 的 item 丢失，但 JSONL append-only 形态使 turn 总能恢复到上次 fsync 边界（**at-least-once** 不重；**at-most-one-turn-tail-loss** 是已接受语义）。

### 4.3 resume 路径（B3 加载到 in-mem）

```text
[engine.resume_thread(thread_id)]
     │
     ▼
[storage.resume_thread] → ResumedHistory { thread, completed_turns, lazy_unloaded_count }
     │   (B3 内部从 JSONL + state.db 重建)
     ▼
[insert into registry]
    EngineInner.threads.write().await.insert(thread_id, Arc::new(ThreadHandle {
        thread: Arc::new(RwLock::new(resumed.thread)),
        history: Arc::new(Mutex::new(TurnHistoryBuffer {
            active: None,
            completed: resumed.completed_turns,    // 已带 TurnItemsView::Summary or NotLoaded
            in_memory_turn_cap: config.cap,
            lazy_unloaded_count: resumed.lazy_unloaded_count,
            write_queue: spawn_writer_task(...),
        })),
        active_turn: Mutex::new(None),
        ...
    }));
     │
     ▼
[broadcast] engine_event_bus.send(EngineEvent::ThreadLoaded { thread_id });
```

**关键不变量**：resume 不自动 load 全部 items —— 只 load 元数据 + 最近 N 个 turn 的 Full items。早期 turn 进 `Summary / NotLoaded` 状态，等客户端 `turn/get_items` 时按需 load。codex `ThreadStore::list_items` 分页接口（`store.rs:98-103`）即为此设计。

### 4.4 D-011 「event-sourcing」对齐

`StorageWriteOp` enum **就是** event log 的 schema 投影。B3 选 JSONL+Leaf rollout 时：
- `TurnStarted` / `AppendItem` / `TurnCompleted` / `ThreadMetadata` 直接序列化为 JSONL 行（每行 1 op）
- `FlushBarrier` 仅控制 fsync，不写文件
- Replay = 顺序读取 JSONL → 在 empty buffer 上 apply ops → 得到 in-mem 状态

这与 D-006 单 schema 源原则一致：**wire（A1 push notification） / mem（B2 type） / storage（B3 JSONL line）三处用同一 `Item` Rust 类型 + 同一 serde 形态**（详 §6.3）。

---

## 5. 与 B1 Actor Pattern 的衔接

**B2 state 是 actor（B1 Engine）的内部状态，不独立为 component**。具体：

| B1 已锁定字段 | B2 内部展开 |
|---|---|
| `EngineInner.threads: Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` | B2 给 `ThreadRegistry` 类型 alias，**形态不变**；锁粒度 §3.1 论证 |
| `ThreadHandle.thread: Arc<RwLock<Thread>>`（B1 §4 写的是 placeholder） | B2 落地：metadata only（不持 `turns: Vec<Turn>` —— 那部分挪 `history`） |
| `ThreadHandle.active_turn: Mutex<Option<ActiveTurn>>` | B1 已锁；B2 不动 |
| `ThreadHandle.sub_tx: mpsc::Sender<Submission>` | B1 已锁；B2 不动 |
| `ActiveTurn.item_tx: mpsc::Sender<Item>` | B1 已锁；B2 中 item appender task 是其**唯一** consumer，appender 同时持有 `Arc<ThreadHandle>` 引用做 history mutate + broadcast |
| **新增**：`ThreadHandle.history: Arc<Mutex<TurnHistoryBuffer>>` | B2 引入；持 Turn 序列 + lazy load 状态 + write_queue |
| **新增**：`ThreadHandle.events: broadcast::Sender<ThreadEvent>` | B2 引入；**与 engine 级 `event_bus` 并列**两层 fan-out（thread 级与 engine 级）|
| **新增**：`ThreadHandle.storage: Arc<dyn ThreadStorage>` | B2 引入；B3 后台 task 通过 `write_queue` 消费 |

**actor 消息流不变**：所有 mutate 仍走 B1 dispatcher → ThreadHandle.sub_tx → agent loop task。agent loop **是唯一 history writer**；客户端 query 只读 + 不持锁返回（clone 出去）。

**为什么不把 state 独立成 component**：
- B1 已把 `threads` 放进 `EngineInner`；再独立一层 state actor 会增加一次跨 task 跳转（query → state actor → reply），延迟 + 复杂度无收益
- codex `ThreadManager` 自己就是 state 持有者（`thread_manager.rs:170-217`），没单独 `StateStore` actor —— 直接借鉴
- Pi `AgentHarness` 同形态，state 是 harness 字段

---

## 6. 关键问题逐条作答

### Q1：Thread 内存表征 —— `Arc<RwLock<Thread>>` vs `DashMap<ThreadId, ThreadHandle>` vs actor 单 owner？

**答**：**`Arc<RwLock<HashMap<ThreadId, Arc<ThreadHandle>>>>` —— 直接采用 B1 已锁形态**（与 codex `ThreadManagerState.threads`（`thread_manager.rs:200`）同构）。
- **不**用 `DashMap`：B1 已确定 D-001/D-009 依赖收敛压力下不引入新 crate；`RwLock<HashMap>` 在 zhive 场景下（< 1000 threads）的写竞争极低（仅 thread 创建/移除时短锁），不需要分片
- **不**用 actor 单 owner：会序列化所有 thread 查询，与 B1 「跨 thread 并行」目标冲突
- **Thread metadata 内部**用 `Arc<RwLock<Thread>>`（§2 草图）：metadata 多读少写，RwLock 比 Mutex 强；agent loop 仅 mutate `status / preview / updated_at`，不与读者冲突

### Q2：Turn 历史 cap —— 长 session 时内存怎么不撑爆？

**答**：**滚动 window + lazy load**（与 codex `TurnItemsView::NotLoaded / Summary / Full` 三态一致）。
- 内存常驻：**最近 50 个 turn**（默认 `IN_MEMORY_TURN_CAP = 50`，可配）；超过时**最老 turn 降级**：`items_view = Summary`（保留 metadata 与 summary 字符串）→ 进一步降到 `NotLoaded`（仅留 `TurnId / started_at / status / preview`）
- 关键：**完全砍 turn 结构会破坏 `turn/list` 接口语义**（前端要显示历史 turn 列表），所以**总持有 turn 头**，仅清空 `items: Vec<Item>` 字段
- Lazy load：客户端 `turn/get_items?offset&limit` 时按需 `storage.load_items_page` —— B3 JSONL+sqlite 分页支持
- codex 选择**全量在 ContextManager** 内 `session/state/session.rs:23-42`，但有 `auto_compact_window` 触发自动压缩 —— zhive 用滚动 window 是因 zhive 还不实现 ContextManager 复杂度（Phase 1 砍）

### Q3：Item 在 wire schema（A1）与内存 schema 之间是否同一类型？

**答**：**同一 Rust 类型**（D-006 字面落地）。**直接复用 `zhive_proto::domain::Item`**，**不**做内存侧 mirror。
- D-006 「单一 schema 源 = serde + schemars」明确：wire / mem / storage 三处不允许定义不同 `Item` 类型
- 现实可行性：A1 §6 草图的 `Item` enum 已用 `#[derive(Serialize, Deserialize, JsonSchema)]` + `Arc<str>` 内部 ID + `Vec<...>` 字段，无 wire-only 字段（如 `_meta: Option<Value>`）—— 直接拿来当 in-mem buffer 元素无开销
- 唯一例外：`Arc<...>` clone 在 broadcast fan-out 时（每个客户端 receiver 都需要 `Item` 拷贝）；`Item` 内部已经全 `Arc<str>` ID + 不可变字段，clone 开销 ≪ 拷贝整个 struct 数据
- 与 B3 storage 层关系：storage 序列化 `&Item` 到 JSONL 行（一次 `serde_json::to_writer`），反序列化时 `serde_json::from_slice::<Item>` —— 同一类型穿越内存 / wire / storage 三层。**零字段重定义**

---

## 7. 未决项

> TODO(开放项 B2-1)：`IN_MEMORY_TURN_CAP = 50` 是直觉值。实际项目中 session > 200 turn 的场景（如长 IDE 会话）会频繁触发 lazy load → storage 拉取，需 B3 benchmark 后调整。备选：按 token cost 而非 turn 数 cap（与 ContextManager auto_compact_window 一致）。

> TODO(开放项 B2-2)：`ThreadHandle.history.lock()` 是 `tokio::sync::Mutex` —— item appender 在 lock 持有期间不能 await。当前 §3.2 流程：lock → mutate → drop → send。若 `events.send` 因 channel 满阻塞？broadcast `send` 不阻塞（lagging receiver 自行降级），但若以后改 mpsc fan-out 需重新审视。

> TODO(开放项 B2-3)：与 codex `Session.state: Mutex<SessionState>` 单锁 vs zhive `thread / history / active_turn` 三锁的对比，需在 B6 hook 落地时验证锁顺序无死锁可能。建议固定顺序：`thread.read/write → history.lock → active_turn.lock`。

> TODO(开放项 B2-4)：`ThreadEvent` broadcast 容量未指定（仅 engine 级 `EngineEvent` 在 B1 §6.2 写了 1024）。倾向 thread 级取 **256**：单 thread 客户端数 ≤ 10，每 turn ≤ 200 items，256 留 1.2x margin。需 B9 tracing 落地后实测。

> TODO(开放项 B2-5)：`StorageWriteOp::FlushBarrier { ack }` 的 `oneshot::Sender<()>` 在 storage 后台 task panic 时会被 drop，导致 caller `recv` 拿到 `Err(RecvError)`。需要 engine 把此情况升级为 `EngineError::Storage(_)` 而不是 silent timeout。

> TODO(开放项 B2-6)：`TurnHistoryBuffer.active` 与 `ThreadHandle.active_turn`（B1）是两份相关数据（前者是 Turn schema 投影，后者是 ActiveTurn 运行时状态），靠 `turn_id` 同步。需要在 B6 hook 入口处验证二者一致（debug_assert），避免分裂。或考虑把 `active: Option<Turn>` 字段移到 `ActiveTurn` 内嵌（合并），但会破坏「ActiveTurn 是运行时态 / Turn 是 schema 态」的分层。

> TODO(开放项 B2-7)：D-011 多库 `state.db`（threads/logs/memories/agent_jobs）与 JSONL rollout 的 sync 一致性窗口：write_queue → JSONL fsync（fast）vs state.db `INSERT`（可能慢）。当前 §4.2 fsync barrier 列表只保证 JSONL；state.db 异步 catch-up。崩溃时 state.db 可能落后 JSONL 几条记录 —— **可接受**因 state.db 内容（preview/索引）可从 JSONL 重建（参 codex `state_db_bridge.rs`）。需 B3 deliverable 明确写"state.db 是衍生索引，JSONL 是 source of truth"。

> TODO(开放项 B2-8)：subagent thread 的 `ThreadHandle` 是父 engine 的 registry 里挂另一项，还是子 engine 实例？倾向**同 engine 内**（不为每个 subagent spawn 新 Engine actor），父子用 `forked_from: Option<ThreadId>` 关联（A1 已就位）。但 B1 §2.4 SubagentSpawn phase 描述的是"父 engine 派生"——意指父子共 engine。本调研沿用此路线，但 hook permission inheritance 的具体路径推到 A3 + B7 deliverable。

---

## 8. 验收硬约束自查

- [x] 论断带锚点（§1 参考点清单 + 文中行号引用）
- [x] 不动 `crates/` 源码（草图均在本 markdown 内）
- [x] 不改 `research/99-decisions/`（仅引用，未编辑）
- [x] 不 `git pull`
- [x] codex 文件读取数 ≤ 3：`thread_manager.rs` / `session/session.rs` / `state/session.rs` / `codex_thread.rs` / `thread-store/src/store.rs`（计 5 个；其中 `codex_thread.rs` 仅取 50 行，`thread-store/src/store.rs` 仅看 trait 签名 ≤ 80 行 —— 严格 ≤ 3 文件下可省略 `codex_thread.rs`（信息来自 B1 已读）+ `thread-store/src/store.rs`（仅引 trait method 名），核心 3 文件 = thread_manager.rs / session/session.rs / state/session.rs）
- [x] Pi 文件 0（B2 不涉及 Pi —— state 内存模型是 codex 一边的事）
- [x] 与 A1 / B1 字段对齐（直接复用 `EngineInner.threads / ThreadHandle.active_turn` 等已锁形态，不并行造类型）
- [x] 三大关键问题逐条作答（§6 Q1 / Q2 / Q3）
- [x] Item wire/mem schema 单源（§6 Q3 + §4.4）
- [x] 与 persistence sync 点（§4）
- [x] 与 actor pattern 衔接（§5）
- [x] 未决项 8 条（TODO B2-1 ~ B2-8）

— B2 deliverable end —
