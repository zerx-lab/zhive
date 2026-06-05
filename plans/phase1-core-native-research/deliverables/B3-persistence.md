---
task: B3
title: Persistence layer — sqlx 4 库 + JSONL+Leaf rollout（D-011 修订版落地）
date: 2026-05-28
status: implemented
depends_on:
  - research/99-decisions D-011（sqlx 多库 + JSONL+Leaf rollout，2026-05-28 修订）
  - A1 deliverable（Thread/Turn/Item schema + turn lifecycle notification 形态）
  - B1 deliverable（Engine actor pattern：`Codex.tx_sub / rx_event` + `ActiveTurn` 边界）
references:
  - ${CODEX}/codex-rs/state/migrations/0001_threads.sql                   (state DB 初始 schema)
  - ${CODEX}/codex-rs/state/logs_migrations/0001_logs.sql                 (logs DB 初始 schema)
  - ${CODEX}/codex-rs/state/memory_migrations/0001_memories.sql           (memories DB 初始 schema)
  - ${CODEX}/codex-rs/state/goals_migrations/0001_thread_goals.sql        (goals DB 初始 schema)
  - ${CODEX}/codex-rs/state/migrations/0014_agent_jobs.sql                (agent_jobs / agent_job_items —— zhive Phase 1 拒收，Phase 2 候选)
  - ${CODEX}/codex-rs/state/src/lib.rs:78-95                              (`SQLITE_HOME_ENV / *_DB_FILENAME / DB_INIT_METRIC` 4 库常量 + 入口 `StateRuntime`)
  - ${CODEX}/codex-rs/state/src/paths.rs:1-10                             (mtime helper —— codex 简单到只导出一个函数)
  - ${CODEX}/codex-rs/rollout/src/recorder.rs:64-78,1348,1615-1625        (RolloutRecorder 形态 + `rollout-<rfc3339>-<uuid>.jsonl` 文件名 + `append_rollout_item_to_path` append-only 写)
  - ${CODEX}/codex-rs/rollout/src/recorder.rs:1632-1660                   (`RolloutLineRef<'a>` 单行 wire 结构 + `write_rollout_item / write_line`)
  - ${CODEX}/codex-rs/rollout/src/list.rs:32-78                           (`ThreadsPage / ThreadItem`：rollout 文件扫描结果)
  - ${CODEX}/codex-rs/rollout/src/session_index.rs:17-65                  (`session_index.jsonl` 附加文件：thread_id ↔ name 映射，append-only)
  - ${PI}/packages/agent/src/harness/session/jsonl-storage.ts:8-15        (`SessionHeader { type:"session", version:3, id, timestamp, cwd, parentSession? }`)
  - ${PI}/packages/agent/src/harness/session/jsonl-storage.ts:109-243     (Leaf 指针读取 / setLeafId append `leaf` 行 / appendEntry 后 currentLeafId 自动 = entry.id)
  - ${PI}/packages/agent/src/harness/types.ts:334-414                     (`SessionTreeEntryBase / MessageEntry / LeafEntry / SessionTreeEntry` union)
  - ${SQL}/Cargo.toml:36-55                                                (rusqlite 0.40 `bundled` feature → `libsqlite3-sys/bundled` 编 C 源码)
  - ${SQL}/src/lib.rs:437-548                                              (`Connection::open / open_with_flags / execute_batch / prepare` —— 0.40 公共 API 仍是单线程 `Connection`，没有内建 pool)
  - ${SQL}/src/lib.rs:1751-1770                                            (`PRAGMA journal_mode` 切换语义)
  - research/99-decisions/README.md:273-313                                (D-011 修订正文：4 库 + Storage trait + Leaf 指针)
  - research/99-decisions/README.md:420-438                                (红线 1 / 红线 8 废除 / 红线 10-11)
---

> **持久化层用 sqlx 0.8**（`SqlitePool`，内建 async 连接池）。这绕开了 rusqlite 路线的两个痛点：R-2（rusqlite `bundled` cold release build 实测 78-80s，超 60s 阈值，详见 §8 研究记录）与 R-7（rusqlite `Connection` 非 Send/Sync 导致的 pool 选型，详见 §6 研究记录）。R-8（跨库一致性）按 D-011 "JSONL source of truth" 给出 fail-strategy + 崩溃恢复伪码（§7）。

---

## 1. 参考点清单

下表是每个论断的锚点（repo + 文件 + 行号），全文逐条引用。

| 主题 | 路径 | 行号 |
|---|---|---|
| codex state DB 初始 schema：`CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, tokens_used, has_user_event, archived, archived_at, git_sha, git_branch, git_origin_url)` + 5 个索引 | `${CODEX}/codex-rs/state/migrations/0001_threads.sql` | 1-25 |
| codex logs DB 初始 schema：`CREATE TABLE logs (id PK AUTOINCREMENT, ts, ts_nanos, level, target, message, module_path, file, line, thread_id, process_uuid, estimated_bytes)` + 5 索引 | `${CODEX}/codex-rs/state/logs_migrations/0001_logs.sql` | 1-22 |
| codex memories DB 初始 schema：`CREATE TABLE stage1_outputs (thread_id PK, source_updated_at, raw_memory, rollout_summary, rollout_slug, generated_at, usage_count, last_usage, selected_for_phase2, selected_for_phase2_source_updated_at)` + `CREATE TABLE jobs (kind, job_key, ..., PRIMARY KEY (kind, job_key))` | `${CODEX}/codex-rs/state/memory_migrations/0001_memories.sql` | 1-36 |
| codex goals DB 初始 schema：`CREATE TABLE thread_goals (thread_id PK, goal_id, objective, status CHECK IN (active/paused/blocked/usage_limited/budget_limited/complete), token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms)` | `${CODEX}/codex-rs/state/goals_migrations/0001_thread_goals.sql` | 1-19 |
| codex agent_jobs 表（Phase 2 候选，zhive 拒）：`CREATE TABLE agent_jobs (...)` + `CREATE TABLE agent_job_items (...)` | `${CODEX}/codex-rs/state/migrations/0014_agent_jobs.sql` | 1-39 |
| codex 4 库常量 + 入口：`SQLITE_HOME_ENV = "CODEX_SQLITE_HOME"` + `LOGS_DB_FILENAME="logs_2.sqlite" / GOALS_DB_FILENAME="goals_1.sqlite" / MEMORIES_DB_FILENAME="memories_1.sqlite" / STATE_DB_FILENAME="state_5.sqlite"` + `pub use runtime::StateRuntime` 作为公开入口 | `${CODEX}/codex-rs/state/src/lib.rs` | 78-95 |
| codex `RolloutRecorder { tx: Sender<RolloutCmd>, writer_task: Arc<RolloutWriterTask>, rollout_path: PathBuf }` —— actor-style writer | `${CODEX}/codex-rs/rollout/src/recorder.rs` | 72-77 |
| codex rollout 文件名：`format!("rollout-{date_str}-{conversation_id}.jsonl")` | `${CODEX}/codex-rs/rollout/src/recorder.rs` | 1348 |
| codex `append_rollout_item_to_path` —— 简单 `.append(true)` JSON line | `${CODEX}/codex-rs/rollout/src/recorder.rs` | 1615-1625 |
| codex `RolloutLineRef<'a>` —— 单行 wire 结构（zhive 抄此 envelope 但叶子换 zhive Item） | `${CODEX}/codex-rs/rollout/src/recorder.rs` | 1632-1660 |
| codex `session_index.jsonl` —— 附加索引文件，`thread_id ↔ name` 映射 append-only，扫描时倒序读 | `${CODEX}/codex-rs/rollout/src/session_index.rs` | 17-65 |
| Pi `SessionHeader { type:"session", version:3, id, timestamp, cwd, parentSession? }` —— JSONL 第 1 行 | `${PI}/packages/agent/src/harness/session/jsonl-storage.ts` | 8-15 |
| Pi `leafIdAfterEntry(entry) = entry.type === "leaf" ? entry.targetId : entry.id` —— **leaf 推导规则**：普通 entry append 后 leaf 自动 = entry.id，**只有 fork / 回放选支才显式写 `leaf` 行** | `${PI}/packages/agent/src/harness/session/jsonl-storage.ts` | 109-111 |
| Pi `setLeafId` —— 显式 append `leaf` 行（含 `parentId = currentLeafId`），用于 fork | `${PI}/packages/agent/src/harness/session/jsonl-storage.ts` | 226-244 |
| Pi `appendEntry` —— 普通 entry append 后 `this.currentLeafId = leafIdAfterEntry(entry)` —— **不写 leaf 行**也能维护 leaf 指针 | `${PI}/packages/agent/src/harness/session/jsonl-storage.ts` | 250-259 |
| Pi `SessionTreeEntryBase { type, id, parentId: string \| null, timestamp }` 公共基类 + `MessageEntry / CompactionEntry / BranchSummaryEntry / LeafEntry / ...` 派生 | `${PI}/packages/agent/src/harness/types.ts` | 334-414 |
| rusqlite 0.40 `bundled` feature → `libsqlite3-sys?/bundled` → 拉 sqlite C amalgamation 进编译 | `${SQL}/Cargo.toml` | 36-55 |
| rusqlite 0.40 公开 API：`Connection::open(path) / open_with_flags / execute_batch / prepare` —— **`Connection` 非 Send/非 Sync**，没有内建 pool | `${SQL}/src/lib.rs` | 437-548 |
| rusqlite 0.40 `PRAGMA journal_mode` 切换：`db.one_column::<String, _>("PRAGMA journal_mode=off", [])` —— 每个 connection 独立设置 | `${SQL}/src/lib.rs` | 1751-1770 |
| D-011 修订正文 + 4 库表 | `research/99-decisions/README.md` | 273-313 |
| 红线 1（禁新依赖）+ 红线 8 废除 + 红线 10-11 | `research/99-decisions/README.md` | 420-438 |
| A1 `Item` 14 case enum + Item builder（落 JSONL 行的叶子内容） | `plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md` §2.1 | 83-98 |

---

## 2. 4 库目录布局（D-011 修订版）

### 2.1 运行时数据目录

```
<base>/                                  # 见下方解析顺序，Linux 默认 = ~/.local/share/zhive/
   state.db                              # threads / sessions / 主索引
   state.db-wal / state.db-shm           # WAL 模式产物（运行时存在）
   logs.db
   logs.db-wal / logs.db-shm
   memories.db
   memories.db-wal / memories.db-shm
   goals.db
   goals.db-wal / goals.db-shm
   rollouts/                             # JSONL source-of-truth（D-011）
      <thread_id>.jsonl                  # 每 thread 一个文件，thread_id 经路径安全化
      session_index.jsonl               # thread_id ↔ name 映射（抄 codex/state_db.rs:1615-1625）
   archived_rollouts/                    # 软删除归档（codex `ARCHIVED_SESSIONS_SUBDIR` 同名）
```

> `<base>` 解析顺序：环境变量 `$ZHIVE_DATA_DIR` → `$XDG_DATA_HOME/zhive` → `$HOME/.local/share/zhive`。`$ZHIVE_DATA_DIR` override 类比 codex `CODEX_SQLITE_HOME`（`state/src/lib.rs:79`）。

### 2.2 Migration 源代码布局

```
crates/zhive-core/migrations/
   state/
      0001_init.sql
      0002_threads_subagent_cwd.sql
   logs/
      0001_init.sql
   memories/
      0001_init.sql
   goals/
      0001_init.sql
```

每个子目录从 1 个 `0001_init.sql` 起步（**对照 codex `goals_migrations/`（1 文件）+ `memory_migrations/`（1 文件）+ `logs_migrations/`（2 文件）+ `migrations/`（35 文件含 backfill）**）。codex `migrations/` 35 文件几乎全是历史演进留痕（`0008_backfill_state.sql / 0009_stage1_outputs_rollout_slug.sql / 0023_drop_logs.sql / 0035_drop_memory_tables.sql` 全是迁移痕迹），zhive **直接落最终态**，每库一个初始迁移即可启动（state 后续追加了 `0002_threads_subagent_cwd.sql` 一条演进）。

> Phase 1 zhive 不复制 codex `agent_jobs / agent_job_items`（`migrations/0014_agent_jobs.sql`），那是 codex 的 CSV 批跑任务专用，与 zhive Phase 1 范围无关。Phase 2 是否引入交 D-010 phase planning。

---

## 3. 每库 `0001_*.sql` 初始 schema

下面 4 段 SQL 全部按 D-006（Thread/Turn/Item）+ A1（Item 14 case enum）合并设计；逐条标注「抄 codex 哪一行」「砍掉哪一列 + 理由」「zhive 新增哪一列」。

### 3.1 `migrations/state/0001_init.sql`

```sql
-- 对照：${CODEX}/codex-rs/state/migrations/0001_threads.sql:1-25
-- 抄：id / created_at / updated_at / source / model_provider / cwd / title
-- 砍：rollout_path（zhive 用 `rollouts/{thread_id}.jsonl` 推导，不存 path）
--     sandbox_policy / approval_mode（Phase 1 不做 sandbox / approval policy）
--     git_sha / git_branch / git_origin_url（Phase 2 加 git_info 时再补）
-- 新增：forked_from_id（Pi 模型，对应 SessionHeader.parentSession）
--       status（A1 TurnStatus 是 Turn 级；这里是 Thread 级生命周期）
--       cli_version（A1 references thread_data.rs:102-148 出现过；用于诊断）
CREATE TABLE threads (
    id              TEXT    PRIMARY KEY,
    forked_from_id  TEXT,                                      -- 父 thread 的 id（fork 时填），对应 Pi parentSession
    title           TEXT    NOT NULL DEFAULT '',
    status          TEXT    NOT NULL CHECK(status IN (
                                'active', 'archived', 'completed', 'errored'
                            )) DEFAULT 'active',
    source          TEXT    NOT NULL,                          -- 'cli' / 'tui' / 'acp_bridge' / 'mcp_bridge' / 'remote'
    model_provider  TEXT    NOT NULL,                          -- 'anthropic' / 'openai' / 'local' / …
    cwd             TEXT    NOT NULL,
    cli_version     TEXT,                                      -- 诊断字段
    tokens_used     INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_threads_created_at ON threads(created_at_ms DESC, id DESC);
CREATE INDEX idx_threads_updated_at ON threads(updated_at_ms DESC, id DESC);
CREATE INDEX idx_threads_status     ON threads(status);
CREATE INDEX idx_threads_source     ON threads(source);
CREATE INDEX idx_threads_forked     ON threads(forked_from_id) WHERE forked_from_id IS NOT NULL;

-- Turn 索引：threads 表外的 turn 元信息（JSONL 是 source-of-truth，
-- 这里是「最后一次 turn 何时开始 / 何时结束」的快速查询表，避免每次扫 JSONL）
-- 对照 A1 §6 Turn 草图：id / status / started_at / completed_at
CREATE TABLE turn_index (
    thread_id     TEXT    NOT NULL,
    turn_id       TEXT    NOT NULL,
    status        TEXT    NOT NULL CHECK(status IN (
                              'in_progress', 'completed', 'interrupted', 'failed'
                          )),
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    PRIMARY KEY (thread_id, turn_id),
    FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
);

CREATE INDEX idx_turn_index_thread_started ON turn_index(thread_id, started_at_ms DESC);
```

> **关键差异**：codex `threads.rollout_path` 是显式列；zhive **不存** —— 用 `rollouts/{thread_id}.jsonl` 推导。这是 D-011 修订条款 "JSONL 是 source of truth，DB 是索引" 的具体落地。

### 3.2 `migrations/logs/0001_init.sql`

```sql
-- 对照：${CODEX}/codex-rs/state/logs_migrations/0001_logs.sql:1-22
-- 抄：全部字段（field-for-field）。codex 的 logs DB 设计已经够薄，没必要重设计。
-- 砍：估算字节 `estimated_bytes` 改 DEFAULT 0 + 不建对应索引（Phase 1 不做 logs 容量统计）
-- 新增：无
CREATE TABLE logs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms           INTEGER NOT NULL,
    ts_nanos        INTEGER NOT NULL,                          -- 同毫秒内的 tie-break
    level           TEXT    NOT NULL,                          -- ERROR/WARN/INFO/DEBUG/TRACE
    target          TEXT    NOT NULL,                          -- tracing target，如 "zhive_core::engine"
    message         TEXT,
    module_path     TEXT,
    file            TEXT,
    line            INTEGER,
    thread_id       TEXT,                                      -- 与 state.threads.id 对照（注意：跨库无 FK）
    process_uuid    TEXT,                                      -- 每次进程启动生成的 uuid
    estimated_bytes INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_logs_ts                              ON logs(ts_ms DESC, ts_nanos DESC, id DESC);
CREATE INDEX idx_logs_thread_id                       ON logs(thread_id);
CREATE INDEX idx_logs_process_uuid                    ON logs(process_uuid);
CREATE INDEX idx_logs_thread_id_ts                    ON logs(thread_id, ts_ms DESC, ts_nanos DESC, id DESC);
CREATE INDEX idx_logs_process_uuid_threadless_ts      ON logs(process_uuid, ts_ms DESC, ts_nanos DESC, id DESC)
    WHERE thread_id IS NULL;
```

> **跨库 FK 限制**：`logs.thread_id` **不能** SQL 层 FK 引用 `state.threads.id`（sqlite 不支持跨 database file 的 FK，ATTACH 也仅能软引用）。zhive 在 repo 层做软校验；删除 thread 时调用层负责级联到 logs（应用代码而非 DB）。详见 §7。

### 3.3 `migrations/memories/0001_init.sql`

```sql
-- 对照：${CODEX}/codex-rs/state/memory_migrations/0001_memories.sql:1-36
-- 抄：表结构（thread_id PK + memory_body 文本字段 + 生成时间），但语义不同：
--     codex `stage1_outputs.raw_memory / rollout_summary` 是后处理产物
--     zhive `memories.body` 是**跨 session 长期记忆**（per Pi MemoryRepo 模式），由
--     extension hook 在 SessionEnd / PreCompact 写入。
-- 砍：`stage1_outputs.usage_count / last_usage / selected_for_phase2*` —— 这些都是 codex
--     phase2 后处理产物，zhive 无对应概念。
-- 砍：`jobs` 表整张 —— codex 用来排队 phase2 任务，zhive Phase 1 无对应 use case。
-- 新增：`kind`（"note" / "fact" / "preference" / "summary"）+ `tags_json`（JSON array）
--       + FTS5 虚表，支持简单 search（Pi MemoryRepo 用 SQLite 也走 FTS5）
CREATE TABLE memories (
    id              TEXT    PRIMARY KEY,                       -- 跨 session 唯一（uuid v7 建议）
    thread_id       TEXT,                                      -- 来源 thread；NULL 表示「全局记忆」
    kind            TEXT    NOT NULL CHECK(kind IN (
                                'note', 'fact', 'preference', 'summary'
                            )),
    body            TEXT    NOT NULL,
    tags_json       TEXT    NOT NULL DEFAULT '[]',             -- JSON array of strings
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_memories_thread     ON memories(thread_id) WHERE thread_id IS NOT NULL;
CREATE INDEX idx_memories_kind       ON memories(kind);
CREATE INDEX idx_memories_updated_at ON memories(updated_at_ms DESC, id DESC);

-- 全文检索（sqlx sqlite 驱动捆绑的 libsqlite3 自带 FTS5）
CREATE VIRTUAL TABLE memories_fts USING fts5(
    body,
    tags,
    content='memories',
    content_rowid='rowid'
);

-- FTS triggers
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, body, tags) VALUES (new.rowid, new.body, new.tags_json);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, body, tags) VALUES ('delete', old.rowid, old.body, old.tags_json);
END;
CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, body, tags) VALUES ('delete', old.rowid, old.body, old.tags_json);
    INSERT INTO memories_fts(rowid, body, tags) VALUES (new.rowid, new.body, new.tags_json);
END;
```

> FTS5 是 sqlite 标准 extension，sqlx sqlite 驱动捆绑的 libsqlite3 自带（编译进 amalgamation），无需额外 feature flag。

> TODO(开放项 B3-3)：Pi `MemoryRepo` 是否分 thread-local / 全局两类？当前 schema 用 `thread_id NULL` 表示全局已能 cover，但 Pi 实际拆了两张表 — 待 A5 / B2 调研补足。

### 3.4 `migrations/goals/0001_init.sql`

```sql
-- 对照：${CODEX}/codex-rs/state/goals_migrations/0001_thread_goals.sql:1-19
-- 抄：thread_id / goal_id / objective / status / token_budget / tokens_used / time_used / 时间戳
-- 砍：`PRIMARY KEY (thread_id)` 单字段 —— 改 `(thread_id, goal_id)` 复合主键，允许
--     一个 thread 多个并行 goal（codex 当前限制每 thread 1 goal，zhive 不抄）
-- 改 status 枚举：去掉 `usage_limited` / `budget_limited`（Phase 1 无 quota 概念），
--                 新增 `cancelled` （取消语义）
CREATE TABLE goals (
    thread_id       TEXT    NOT NULL,
    goal_id         TEXT    NOT NULL,
    objective       TEXT    NOT NULL,
    status          TEXT    NOT NULL CHECK(status IN (
                                'active', 'paused', 'blocked', 'complete', 'cancelled'
                            )) DEFAULT 'active',
    token_budget    INTEGER,                                   -- NULL = unlimited
    tokens_used     INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (thread_id, goal_id)
);

CREATE INDEX idx_goals_thread_status   ON goals(thread_id, status);
CREATE INDEX idx_goals_status_updated  ON goals(status, updated_at_ms DESC);
```

---

## 4. JSONL rollout：文件结构 + Leaf entry schema

### 4.1 文件路径布局

每 thread 一个文件，文件名仿 codex `recorder.rs:1348`（`rollout-<rfc3339-with-dashes>-<thread_id>.jsonl`），但 zhive **简化为** `<thread_id>.jsonl`：

```
$XDG_DATA_HOME/zhive/rollouts/
    <thread_id>.jsonl            # 每 thread 一个，append-only
    session_index.jsonl          # thread_id ↔ name 映射（抄 codex/session_index.rs:17-65）
```

**理由**：
- codex 把 RFC3339 时间戳放文件名是为「按时间扫盘」list 优化，但 zhive 已经有 `state.db` 做 list 索引（D-011），文件名里再放时间戳是冗余且有时区坑（codex 用 `T17-24-21` 不带 zone）。
- 文件名 = thread_id 在 `find / ls` / 备份脚本时也更直观。
- 副作用：失去「按文件名快速排序」能力，但已被 `threads.created_at_ms` index 替代。

### 4.2 第 1 行：`SessionHeader`

照 Pi `jsonl-storage.ts:8-15` schema，**字段名锁定（不重命名）** 以便未来直接 borrow Pi 的 storage 类（zhive Rust 端 / 未来 ts 端共享 schema）：

```jsonc
{
  "type": "session",
  "version": 4,                         // 当前 schema 版本（Wave4 起 v4）
  "id": "thread:native/0190...",        // thread_id
  "timestamp": 1748443041,              // Unix 秒
  "cwd": "/home/user/project",
  "parentSession": "thread:native/...", // optional，fork 来源 thread_id
  "subagentParent": "thread:native/...",// optional，作为 subagent 被 spawn 时的父 thread
  "source": "user"                      // optional，thread 来源：user / subagent / memory_consolidation
}
```

### 4.3 后续行：`SessionTreeEntry` union

照 Pi `types.ts:334-414` 的 `SessionTreeEntryBase` 模式，但 entry 内容用 **A1 deliverable §2.1 的 14-case `Item` enum**（参考 `A1-thread-turn-item.md:83-98`）。

每行用 `type` 作判别符（snake_case 值），其余字段随 case 而定：

```jsonc
{
  "type": "<entry_type>",        // 见下面 case 列表
  "threadId": "thread:native/...",// 多数 case 带（leaf 例外）
  "turnId": "...",                // item / compaction / pending_permission 带
  "timestamp": 1748443042,        // Unix 秒
  ...case-specific...
}
```

**zhive rollout 的 entry types**（`RolloutEntry`）：

| type | 携带数据 | 触发时机 |
|---|---|---|
| `session` | `version / id / timestamp / cwd / parentSession? / subagentParent? / source?` | 文件第 1 行（§4.2） |
| `item` | `threadId / turnId / timestamp`；A1 `Item`（含 `itemKind` 判别符） | Engine 每次 emit item 时（B1 `tx_event`） |
| `compaction` | `threadId / turnId / timestamp / summary / replacement: [Item] / entriesCompacted` | 上下文压缩 checkpoint（Pi `CompactionEntry` 同源，types.ts:357-363） |
| `pending_permission` | `threadId / turnId / timestamp / requestId / request` | turn 因 `Defer` 权限决策挂起（B6），resume 时重新 surface |
| `permission_resolved` | `threadId / requestId / timestamp` | 挂起的权限请求被应答 / turn 取消（B6），supersede 对应 `pending_permission` |
| `leaf` | `targetId: string \| null` | **仅 fork / 回放选支时写**；普通 append 不写（Pi 规则，§4.4 解释） |

> turn 生命周期（start / end / status）不进 rollout，而是落在 state.db 的 `turns` 表（§3）；rollout 只承载上面这些内容流 + 控制行。

### 4.4 Leaf 指针写入策略

**Pi 规则**（`jsonl-storage.ts:109-111, 250-259`）：
```ts
function leafIdAfterEntry(entry) {
  return entry.type === "leaf" ? entry.targetId : entry.id;
}

// 普通 appendEntry 后：
this.currentLeafId = leafIdAfterEntry(entry);  // = entry.id
// → 不写 leaf 行也能正确维护 leaf 指针
```

**zhive 决定照抄**：
- 普通 append → leaf 隐式 = 最新 entry.id（**不写 leaf 行**，节省 80% 的 leaf 行噪声）
- fork / `setLeafId(targetId)` 时 → **显式写一行** `{"type":"leaf","targetId":"<entry-id>",parentId:<old leaf>,...}`
- fork 后原 leaf **保留**：fork 等价于「换 leaf 指针指向旧分支某点」，原本属于旧分支的 entry 全部保留在 jsonl 里，可后续通过 `getPathToRoot(leafId)` 找回（Pi `jsonl-storage.ts:275-288`）。

**fork 流程伪码**（zhive 等价）：
```rust
// 1. 用户在 entry X 处发起 fork
// 2. 不开新 jsonl 文件，继续 append 到当前 thread 的 jsonl
// 3. 显式写 leaf row 切换 leaf 指针到 X：
storage.set_leaf_id(Some(entry_x_id))?;
// 4. 之后新 entry 的 parent_id = X，构成新分支
// 5. 旧分支的 leaf 不丢，可通过遍历 entries 找到（Pi getEntries + getPathToRoot）
```

> 这一选择与 codex `RolloutRecorder` 的差异：codex **不做 leaf 指针 / fork 树**，每次 fork 新 thread 都开新 jsonl 文件（`forked_from_id` 跨文件指）。zhive 选 Pi 模型是因为 D-011 修订正文明确「Leaf 指针采纳 Pi 模型」。

---

## 5. `Storage` trait + 4 子 trait 草图

> 本节是 trait + 方法签名草图。落地后 `crates/zhive-core/src/persistence/` 用聚合 struct `Storage`（持 4 个具体 `StateDb / LogsDb / MemoriesDb / GoalsDb`）+ 一个 `ThreadStorage`（RPITIT，可 mock）承载下面这些操作。

```rust
//! crates/zhive-core/src/persistence/mod.rs

/// 4 库聚合接口（D-011 §决策）。
///
/// 实现层握有 4 个 [`sqlx::SqlitePool`]（每库一文件一池），按需借 connection。
///
/// **跨库一致性**：本 trait **不提供** 跨库事务原语（sqlite `BEGIN` 是单 DB
/// 文件作用域，ATTACH 上来的库在 WAL 下默认 read-only，见 §7）。调用者必须遵循
/// §7 的 "JSONL source of truth" fail-strategy。
pub trait Storage: Send + Sync {
    fn state(&self) -> &dyn StateDb;
    fn logs(&self) -> &dyn LogsDb;
    fn memories(&self) -> &dyn MemoriesDb;
    fn goals(&self) -> &dyn GoalsDb;

    /// 用于崩溃恢复 / 备份 / 单元测试。返回 4 个 DB 文件路径。
    fn db_paths(&self) -> StorageDbPaths;
}

#[derive(Debug, Clone)]
pub struct StorageDbPaths {
    pub state: std::path::PathBuf,
    pub logs: std::path::PathBuf,
    pub memories: std::path::PathBuf,
    pub goals: std::path::PathBuf,
}

// ---- StateDb（state.db） ----

#[async_trait::async_trait]
pub trait StateDb: Send + Sync {
    /// 创建新 thread。返回该 thread 的稳定 id。
    async fn create_thread(&self, args: CreateThreadArgs) -> Result<ThreadId, StorageError>;

    /// 列出 thread（分页 + status 过滤）。**JSONL 是 source of truth**：
    /// 这里只返回索引视图（标题、created_at 等），不还原全部 item。
    async fn list_threads(&self, q: ThreadQuery) -> Result<ThreadPage, StorageError>;

    /// 取单 thread 元信息（不含 item / turn 历史）。
    async fn get_thread(&self, id: &ThreadId) -> Result<Option<ThreadMeta>, StorageError>;

    /// 新 turn 入索引（不存 turn 内 item；item 流落 JSONL）。
    async fn record_turn_start(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        started_at_ms: i64,
    ) -> Result<(), StorageError>;

    /// turn 结束时更新索引。
    async fn record_turn_end(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        status: TurnStatus,
        completed_at_ms: i64,
    ) -> Result<(), StorageError>;

    /// 软删除 thread（status -> 'archived'）。**不级联** logs / memories / goals。
    async fn archive_thread(&self, id: &ThreadId) -> Result<(), StorageError>;
}

// ---- LogsDb（logs.db） ----

#[async_trait::async_trait]
pub trait LogsDb: Send + Sync {
    /// 单条 log 落盘。`thread_id == None` 表示 process-level log（如 startup）。
    async fn record_log(&self, log: LogEntry) -> Result<(), StorageError>;

    /// 批量 log（高频路径走批写）。
    async fn record_logs(&self, logs: Vec<LogEntry>) -> Result<(), StorageError>;

    /// 查询 log（按 thread / level / 时间范围）。
    async fn query_logs(&self, q: LogQuery) -> Result<Vec<LogRow>, StorageError>;

    /// 清理：删除 ts_ms < cutoff 的 log（运维用）。
    async fn purge_logs_before(&self, cutoff_ms: i64) -> Result<u64, StorageError>;
}

// ---- MemoriesDb（memories.db） ----

#[async_trait::async_trait]
pub trait MemoriesDb: Send + Sync {
    /// upsert：若 id 已存在则更新 body / tags / updated_at_ms。
    async fn upsert_memory(&self, mem: Memory) -> Result<(), StorageError>;

    /// 按 id 取单条。
    async fn get_memory(&self, id: &MemoryId) -> Result<Option<Memory>, StorageError>;

    /// FTS5 search：`query` 走 sqlite MATCH 语法。
    async fn search_memories(&self, q: MemoryQuery) -> Result<Vec<Memory>, StorageError>;

    /// 删除单条。
    async fn delete_memory(&self, id: &MemoryId) -> Result<(), StorageError>;
}

// ---- GoalsDb（goals.db） ----

#[async_trait::async_trait]
pub trait GoalsDb: Send + Sync {
    /// 新增 goal。复合主键 (thread_id, goal_id) 冲突时由底层 sqlx 抛 `StorageError::Sqlx`。
    async fn add_goal(&self, goal: Goal) -> Result<(), StorageError>;

    /// 标记 goal 完成（status -> 'complete'）+ 更新 updated_at_ms。
    async fn mark_done(&self, thread_id: &ThreadId, goal_id: &GoalId) -> Result<(), StorageError>;

    /// 列 goal（按 thread / status）。
    async fn list_goals(&self, q: GoalQuery) -> Result<Vec<Goal>, StorageError>;

    /// 累加 tokens_used（每 turn 结束时调用）。
    async fn add_tokens_used(
        &self,
        thread_id: &ThreadId,
        goal_id: &GoalId,
        delta: u64,
    ) -> Result<(), StorageError>;
}

// ---- 共享类型（占位；具体字段沿 A1 / D-011） ----

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum StorageError {
    #[error("sqlx error: {0}")] Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")] Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io error: {0}")] Io(#[from] std::io::Error),
    #[error("json error: {0}")] Json(#[from] serde_json::Error),
    #[error("rollout corrupted at line {line}: {reason}")]
    RolloutCorrupted { line: usize, reason: String },
}

pub type ThreadId = String;          // 占位；真正定义在 A1
pub type TurnId   = String;
pub type GoalId   = String;
pub type MemoryId = String;

// CreateThreadArgs / ThreadQuery / ThreadPage / ThreadMeta /
// LogEntry / LogQuery / LogRow / Memory / MemoryQuery / Goal / GoalQuery
// 的字段对照 §3 SQL schema 镜像。略。
```

### 5.1 与 codex `state/src/lib.rs` 公开类型的逐项对照

| zhive | codex | 取舍 |
|---|---|---|
| `Storage` 聚合 struct（持 4 个具体 DB） | codex `StateRuntime` （`lib.rs:21,46`）—— 同为聚合 struct | 形态一致；zhive 额外抽 `ThreadStorage`（RPITIT trait）供 in-memory mock 注入 |
| `StateDb / LogsDb / MemoriesDb / GoalsDb` 4 个具体 struct | codex `MemoryStore / GoalStore`（`lib.rs:55-58`）+ `log_db` module —— **2 trait + 1 module，未对齐** | zhive 4 struct 对齐 4 库，工程感更整齐 |
| `record_log` / `query_logs` | codex `log_db::*`（`state/src/log_db.rs`） | 同语义 |
| `upsert_memory / search_memories` | codex `MemoryStore` | 同语义 |
| `add_goal / mark_done` | codex `GoalStore / GoalUpdate / GoalAccountingMode`（`lib.rs:54-57`） | zhive 砍 `GoalAccountingMode`（codex 用于 token 限额，Phase 1 不引入） |
| `forked_from_id` 字段 | codex 无对应（codex 用 `threads.rollout_path` 中文件命名传） | zhive 抄 Pi `parentSession` |
| `agent_jobs / agent_job_items` 表 | codex 有（`migrations/0014_agent_jobs.sql`） | **zhive Phase 1 拒**（CSV batch job 非范围） |
| `stage1_outputs / jobs / phase2_*` | codex `memory_migrations/0001_memories.sql` | **zhive 拒**（codex 后处理流水线，与 zhive memories 语义不同） |

---

## 6. Connection pool（sqlx `SqlitePool`，每库一池）

### 6.1 sqlx 0.8 自带 async 连接池

持久化层用 sqlx 0.8 的 `SqlitePool`（内建 async 连接池，tokio-native）。每库一个文件、一个 pool：`StateDb / LogsDb / MemoriesDb / GoalsDb` 各自握有一个 `SqlitePool`，通过 `SqlitePool::connect_with(SqliteConnectOptions)` 打开。

> **R-7 研究记录**：rusqlite 路线下 `Connection` 是 `!Send + !Sync`（内部用 `RefCell` 包裹原始指针，见 `${SQL}/src/lib.rs:437-548`），多线程访问需要自选 pool 策略（r2d2-sqlite / deadpool-sqlite 触发禁新依赖红线，或自写 `Arc<Mutex<Connection>>` mini pool），这正是促成改用 sqlx 的原因之一 —— sqlx `SqlitePool` 提供原生 async 池，无需引入额外 pool crate，也不触发 CLAUDE.md 红线 1。

### 6.2 连接选项（WAL + NORMAL + foreign_keys）

每库 `SqliteConnectOptions` 设置一致，由 sqlx 在建立 connection 时应用：

```rust
let opts = SqliteConnectOptions::new()
    .filename(path)
    .create_if_missing(true)
    .journal_mode(SqliteJournalMode::Wal)
    .synchronous(SqliteSynchronous::Normal)   // WAL 配 NORMAL 即可，FULL 太慢
    .foreign_keys(true);                       // state.db 跨表 FK 级联需要
let pool = SqlitePool::connect_with(opts).await?;
```

- **WAL 模式**：sqlite WAL 允许多 reader + 1 writer 并行（这是 WAL 比 rollback journal 的核心优势）；pool 内的连接共享同一份 `*.db-wal` 文件。
- **`synchronous=NORMAL`**：配 WAL 的安全/吞吐折中点；FULL 在每次提交都 fsync，太慢。
- **`foreign_keys=ON`**：sqlite 默认关闭 FK 强制；state.db 的 `turn_index → threads` 级联删除依赖它，故每个 connection 都需开启。
- **每库独立 pool，不共享**：4 库是 4 个文件，sqlite WAL 锁是文件级；跨文件共享一个 pool 无意义（池里的连接各指不同文件，不能复用）。

---

## 7. 跨库一致性策略（R-8）

### 7.1 跨库事务原子性：**不保证**

sqlite 的 `BEGIN TRANSACTION` 是单 DB 文件作用域（sqlx 与任何 sqlite 驱动同此）。多 DB 想原子写有三个理论方案：

1. **sqlite ATTACH DATABASE**：把 4 个文件 ATTACH 到同一 main DB，写事务跨 ATTACH。
   - 限制：WAL 模式 + ATTACH 的写事务**仅 main DB 可写**，ATTACH 上来的库默认 read-only（[sqlite WAL doc](https://www.sqlite.org/wal.html) §10）。
   - 即使关 WAL 也会有性能 + 死锁回退路径，**不采纳**。
2. **应用层 2PC**：复杂；与 D-009 "尽量简" 矛盾，**不采纳**。
3. **JSONL source of truth + DB 异步重建**（D-011 修订正文采纳）：DB 仅是索引，原子性由 JSONL 单文件 append 保证。**zhive 选此**。

### 7.2 Fail-strategy：JSONL 先写成功，DB 失败可异步重建

**写顺序硬约定**（B1 actor 内强制）：

```text
顺序     操作                                        失败处理
1.     append JSONL（fsync 完成后才算 OK）         失败 → 整体 fail，向上抛 StorageError
2.     state.record_turn_start（索引）              失败 → log warn + 标记 thread "needs_rebuild"
3.     logs.record_log（如果 turn 写 log）          失败 → log warn + 跳过（log 是可丢的）
4.     memories.upsert_memory（如果有）             失败 → log warn + 标记 memory_id "pending_retry"
5.     goals.add_tokens_used（如果有 active goal）  失败 → log warn + token 计数有偏差但不 fatal
```

**核心论断**：JSONL 写成功 = 用户事实已成立；后续 4 个 DB 写都是 "缓存预热"，失败不影响数据正确性，只影响**查询速度**和**统计精度**。

### 7.3 崩溃恢复伪码（从 JSONL 重建 4 DB）

```rust
/// 启动时 + 显式调用 `zhive rebuild-indexes` 时执行。
/// 复杂度 O(N) where N = JSONL 总行数。
async fn rebuild_indexes_from_jsonl(
    rollouts_dir: &Path,
    storage: &dyn Storage,
) -> Result<RebuildStats, StorageError> {
    let mut stats = RebuildStats::default();
    for entry in std::fs::read_dir(rollouts_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if path.file_name() == Some(OsStr::new("session_index.jsonl")) {
            continue;  // 索引文件，跳过
        }

        // 1. 解析 header（第一行 = RolloutEntry::Session）
        let mut reader = BufReader::new(File::open(&path)?);
        let mut header_line = String::new();
        reader.read_line(&mut header_line)?;
        let header: RolloutEntry = serde_json::from_str(&header_line)?;

        // 2. upsert thread metadata（state.db）
        storage.state().upsert_thread_from_header(&header).await?;

        // 3. 逐行重放 entry，按 RolloutEntry 变体分派
        for line in reader.lines() {
            let entry: RolloutEntry = serde_json::from_str(&line?)?;
            match entry {
                RolloutEntry::Item { thread_id, turn_id, ref item, .. } => {
                    // 重建 state.db 的 turns / items 索引（item 全文留在 JSONL）
                    storage.state().index_item(&thread_id, &turn_id, item).await?;
                }
                RolloutEntry::Compaction { thread_id, turn_id, .. } => {
                    // 压缩 checkpoint：重放 turn 元信息并安装 replacement
                    storage.state().index_compaction(&thread_id, &turn_id, &entry).await?;
                }
                RolloutEntry::Leaf { .. }
                | RolloutEntry::PendingPermission { .. }
                | RolloutEntry::PermissionResolved { .. }
                | RolloutEntry::Session { .. } => {
                    // 控制行 / header：不重建额外 DB 表（leaf 指针与权限恢复
                    // 由 resume 路径单独处理）
                }
            }
            stats.entries_replayed += 1;
        }
        stats.threads_rebuilt += 1;
    }
    Ok(stats)
}
```

### 7.4 logs.db 重建说明

**logs.db 不从 JSONL 重建** —— logs 本身就是「丢失可接受」的运维数据，JSONL 里**不存** log 行（logs 与 thread item 流是不同语义）。崩溃时 logs.db 损坏 → 直接重建空 schema，历史 log 丢失。

### 7.5 memories.db 重建说明

**memories.db 部分可从 JSONL 重建** —— 仅 `kind="summary"` 且来源是 compaction entry 的可重建；用户手工写的 `note / fact` 类记忆只在 memories.db 里，**没有 source of truth**。

> TODO(开放项 B3-1)：是否给用户手写 memory 也加一份 JSONL 落盘（如 `memories.jsonl`）？这样实现真正的「JSONL 为唯一 source of truth」。当前方案保留 memories.db 损坏 = 用户手写记忆丢失 的语义，**接受这个 trade-off**。

---

## 8. R-2 研究记录：rusqlite 0.40 + bundled 编译成本

> **本节是 R-2 研究证据**，记录 rusqlite `bundled` 路线的编译成本测量；这条成本是 D-011 锁定 sqlx 的依据之一。实测于 **2026-05-28**，使用临时 probe crate `crates/zhive-persistence-probe/`（已删除）。

### 8.1 测试方法

probe crate 设计（已 cleanup）：
- 标准独立 cargo project（顶层 `[workspace]` 表阻断 zhive workspace 影响）
- 依赖 `rusqlite = { version = "=0.40.0", features = ["bundled"] }`
- main 函数：open 4 个 `:memory:` SQLite，跑 4 段 DDL，记录耗时

测试机：CachyOS Linux 6.x，`CARGO_INCREMENTAL=0`（避免与 sccache 冲突，per memory），无 mold / sccache / pre-built artifacts。每个数字都是 `rm -rf target && date +%s%N → cargo build → date +%s%N`。

### 8.2 实测结果

| 指标 | 数值 | 备注 |
|---|---|---|
| **cold release build (`cargo build --release`)** | **78-80s** | 两次独立 run：80s + 78s |
| cold dev build (`cargo build`) | 10-11s | -O0 不优化 sqlite C |
| incremental rebuild (top crate only touch) | 0.6s | 仅 probe crate 重编 |
| `--release` binary size (未 strip) | **2.7 MiB** (apparent 2.5 MiB) | `du -h --apparent-size` |
| `--release` binary size (`strip` 后) | 2.5 MiB | `strip` symbols |
| 4 个 `:memory:` open + DDL bootstrap | **1.52 ms** | runtime cost |

### 8.3 R-2 判定

> **R-2 触发**：rusqlite `bundled` 路线 cold release build **78-80s > 60s 阈值**（plan §9 R-2 阈值）。**未达 >2min 极端值**，但显著超过 60s。

probe 的空 main 已要 78-80s，叠加 zhive-core 既有的 tokio / serde / schemars 等大依赖后，rusqlite+bundled 下的 zhive-core 单 crate cold release 预估会进一步抬到 90-150s，外加二进制 +2.5 MiB（libsqlite3 C 编译产物）。**这条编译/体积成本是 D-011 锁定 sqlx 的关键依据** —— sqlx 用预编译的 sqlite 驱动，cold build 不背 C amalgamation 编译。

### 8.4 rusqlite 路线本需要的缓解清单

下列缓解专属于 rusqlite+bundled 路线（背 C amalgamation 编译时才相关）：

1. **CI 配 sccache**（已部署，per memory `feedback-sccache-incremental.md`）—— sqlite C 编译产物可 cache。
2. **本机开发用 dev profile**（cold = 11s OK）+ release 仅在打包 / dist 时触发。
3. **不切 `bundled-sqlcipher`**（会拉 OpenSSL，体积 +5MB / cold +30s）。
4. **关闭 rusqlite 默认不需要的 feature**：只用 `bundled`，不开 `vtab / array / functions / hooks / chrono` 等。
5. 长期可考虑 `libsqlite3-sys` 改链系统 sqlite3 `.so` —— 砍编译时间，但牺牲 Windows / macOS 用户体验。

---

## 9. 与 codex 当前实现的逐项对照

| 项 | codex（当前主线） | zhive Phase 1 | 取舍 |
|---|---|---|---|
| ORM | `sqlx` (`state/Cargo.toml`) | `sqlx 0.8`（`SqlitePool`，内建 async 池） | D-011 修订锁定 sqlx，理由：`sqlx::migrate!` 可嵌入 SQL 文件 + 原生 async 池免引 pool crate |
| 库分离 | 4 个文件，35+2+1+1 migrations | 4 个文件，2+1+1+1 migration（终态直落） | zhive 直接抄结构但跳过 codex 演进留痕 |
| DB 文件名 | `state_5.sqlite / logs_2.sqlite / memories_1.sqlite / goals_1.sqlite`（带版本号） | `state.db / logs.db / memories.db / goals.db`（无版本号） | zhive schema version 由 migration table 自管，文件名不带版本（便于 backup tooling） |
| rollout 文件 | `rollout-<rfc3339>-<thread_id>.jsonl` | `<thread_id>.jsonl` | 文件名简化（理由 §4.1） |
| session_index | `session_index.jsonl` append-only | 同 | 抄 |
| `threads.rollout_path` 列 | 有 | **无** | zhive 用文件名推导（D-011 "JSONL 是 source of truth" 强约定） |
| `threads.sandbox_policy / approval_mode` 列 | 有 | 无 | Phase 1 不做 sandbox / approval policy |
| `threads.git_*` 列 | 有 | 无（Phase 2） | scope 控制 |
| `agent_jobs / agent_job_items` 表 | 有（migrations/0014） | 无 | CSV batch 不在 zhive Phase 1 范围 |
| `stage1_outputs / jobs` 表（memories DB） | 有 | 无 | codex 后处理流水线，zhive 不抄 |
| `goals.PRIMARY KEY` | `(thread_id)` 单字段 → 限制 1 thread 1 goal | `(thread_id, goal_id)` 复合 → 多 goal 并行 | zhive 放宽 |
| `goals.status` 枚举 | 6 态（含 `usage_limited / budget_limited`） | 5 态（去 quota，加 `cancelled`） | Phase 1 无 quota |
| Leaf 指针 | 无（codex fork = 新 jsonl 文件） | 有（D-011 修订条款，Pi 模型） | zhive 选 Pi |
| FTS5 memory 搜索 | 无（codex 用关键字 LIKE） | 有 | zhive 加 FTS5（sqlx sqlite 驱动捆绑的 libsqlite3 自带） |
| Connection 抽象 | sqlx Pool | sqlx `SqlitePool`（每库一池） | 同选 sqlx 内建池 |

---

## 10. R-2 / R-7 / R-8 触发与缓解

| 风险 | 触发 | 本 deliverable 给出的缓解 |
|---|---|---|
| **R-2**（rusqlite bundled cold build） | 已规避 | 改用 sqlx 后不再背 C amalgamation 编译；§8 保留 rusqlite 路线的测量作为决策依据 |
| **R-7**（pool 新依赖红线） | 已规避 | sqlx `SqlitePool` 提供原生 async 池，无需 r2d2/deadpool 等 pool crate，不触发红线 1（§6） |
| **R-8**（跨库一致性） | ✅ 触发（4 库无原子事务） | §7 fail-strategy：JSONL source of truth + 异步重建索引；崩溃恢复伪码已落地 §7.3 |

---

## 11. 未决项

> 全部按 plan §10 回流原则待补到 D-011 / B3 实施任务。

- **TODO(开放项 B3-1)**：用户手写 memory（`kind="note" / "fact"`）是否也走 JSONL 落盘 → 当前方案接受 memories.db 损坏 = 手写丢失。**待用户决策**：是否新增 `memories.jsonl` source of truth？
- **TODO(开放项 B3-3)**：Pi `MemoryRepo` 是否分 thread-local / 全局两类？zhive 目前 schema 用 `thread_id NULL` 表示全局，等 A5 / B2 调研最终拍。
- **TODO(开放项 B3-5)**：`zhive-core` 把 4 库 + rollout 全揉一起，是否拆 `zhive-persistence` 子 crate（违反 D-001 "7 crate 起步" 约束）？或继续单 crate？
- **TODO(开放项 B3-6)**：cross-DB ID 跨域映射工具：`logs.thread_id ↔ state.threads.id` 软关系，建议落「ID newtype + 跨 trait 校验函数」防止 lint 漏过。

---

## 12. 一句话总结（plan §10 回流摘要）

zhive Phase 1 直接落 4 库（state / logs / memories / goals）+ JSONL Leaf rollout，用 sqlx 0.8 + 每库一个 `SqlitePool`（内建 async 池）；每库 `0001_init.sql` 已对照 codex 同名 migration 写好（state 后续加了 `0002_threads_subagent_cwd.sql`）；`Storage` 聚合 struct + `ThreadStorage`（RPITIT，可 mock）+ cross-DB fail-strategy（JSONL source of truth + 异步 DB 重建）已就位；选 sqlx 同时规避了 R-2（rusqlite bundled cold build）与 R-7（rusqlite `Connection` 非 Send/Sync 的 pool 选型）两条研究风险。
