---
task: B3
title: Persistence layer — rusqlite 4 库 + JSONL+Leaf rollout（D-011 修订版落地）
date: 2026-05-28
status: draft
depends_on:
  - research/99-decisions D-011（rusqlite 多库 + JSONL+Leaf rollout，2026-05-28 修订）
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

> **R-2 触发**：rusqlite =0.40 + `bundled` cold release build **实测 78-80s**，超过 60s 阈值。详见 §8。**建议向用户确认是否接受**，缓解措施在 §11 列。R-7（pool 选型）三方案对照见 §6，**未自行决策**，**等用户拍**。R-8（跨库一致性）按 D-011 "JSONL source of truth" 给出 fail-strategy + 崩溃恢复伪码（§7）。

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
$XDG_DATA_HOME/zhive/                    # Linux 默认 = ~/.local/share/zhive/
   db/
      state.db                           # threads / sessions / 主索引
      state.db-wal / state.db-shm        # WAL 模式产物（运行时存在）
      logs.db
      logs.db-wal / logs.db-shm
      memories.db
      memories.db-wal / memories.db-shm
      goals.db
      goals.db-wal / goals.db-shm
   rollouts/                              # JSONL source-of-truth（D-011）
      rollout-2026-05-28T14-37-21-<uuid>.jsonl
      rollout-...jsonl
      session_index.jsonl                # thread_id ↔ name 映射（抄 codex/state_db.rs:1615-1625）
   archived_rollouts/                     # 软删除归档（codex `ARCHIVED_SESSIONS_SUBDIR` 同名）
```

> 路径 override：环境变量 `ZHIVE_DATA_HOME`，类比 codex `CODEX_SQLITE_HOME`（`state/src/lib.rs:79`）。

### 2.2 Migration 源代码布局

```
crates/zhive-core/migrations/
   state/
      0001_threads.sql
   logs/
      0001_logs.sql
   memories/
      0001_memories.sql
   goals/
      0001_goals.sql
```

每个子目录 1 个文件起步（**对照 codex `goals_migrations/`（1 文件）+ `memory_migrations/`（1 文件）+ `logs_migrations/`（2 文件）+ `migrations/`（35 文件含 backfill）**）。codex `migrations/` 35 文件几乎全是历史演进留痕（`0008_backfill_state.sql / 0009_stage1_outputs_rollout_slug.sql / 0023_drop_logs.sql / 0035_drop_memory_tables.sql` 全是迁移痕迹），zhive **直接落最终态**，4 个 0001 文件即可启动。

> Phase 1 zhive 不复制 codex `agent_jobs / agent_job_items`（`migrations/0014_agent_jobs.sql`），那是 codex 的 CSV 批跑任务专用，与 zhive Phase 1 范围无关。Phase 2 是否引入交 D-010 phase planning。

---

## 3. 每库 `0001_*.sql` 初始 schema

下面 4 段 SQL 全部按 D-006（Thread/Turn/Item）+ A1（Item 14 case enum）合并设计；逐条标注「抄 codex 哪一行」「砍掉哪一列 + 理由」「zhive 新增哪一列」。

### 3.1 `migrations/state/0001_threads.sql`

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

### 3.2 `migrations/logs/0001_logs.sql`

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

> **跨库 FK 限制**：`logs.thread_id` **不能** SQL 层 FK 引用 `state.threads.id`（rusqlite 不支持跨 database file 的 FK，sqlite ATTACH 也仅能软引用）。zhive 在 ORM/repo 层做软校验；删除 thread 时调用层负责级联到 logs（应用代码而非 DB）。详见 §7。

### 3.3 `migrations/memories/0001_memories.sql`

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

-- 全文检索（rusqlite 0.40 bundled 自带 FTS5）
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

> FTS5 是 sqlite 标准 extension，rusqlite 0.40 + `bundled` feature 自带（编译进 amalgamation），无需额外 feature flag。

> TODO(开放项 B3-3)：Pi `MemoryRepo` 是否分 thread-local / 全局两类？当前 schema 用 `thread_id NULL` 表示全局已能 cover，但 Pi 实际拆了两张表 — 待 A5 / B2 调研补足。

### 3.4 `migrations/goals/0001_goals.sql`

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
  "version": 1,                         // zhive 起 v1（Pi 是 v3，因 Pi 已经经历 2 次 schema break）
  "id": "01HXYZ...",                    // thread_id（uuid v7 建议）
  "timestamp": "2026-05-28T14:37:21Z",  // RFC3339 UTC
  "cwd": "/home/user/project",
  "parentSession": "01HX..."            // optional，fork 来源 thread_id
}
```

### 4.3 后续行：`SessionTreeEntry` union

照 Pi `types.ts:334-414` 的 `SessionTreeEntryBase` 模式，但 entry 内容用 **A1 deliverable §2.1 的 14-case `Item` enum**（参考 `A1-thread-turn-item.md:83-98`）。

公共 envelope：

```jsonc
{
  "type": "<entry_type>",        // 见下面 case 列表
  "id": "01HX...",                // 该 entry 自己的 id（rolloutLine id，非 turn_id / item_id）
  "parentId": "01HX..." | null,   // 父 entry id（链表形式构造 tree —— fork 时 parentId 跨分支跳跃）
  "timestamp": "2026-05-28T14:37:22Z",
  ...case-specific...
}
```

**zhive 实测必需的 entry types**：

| entry.type | 携带数据 | 触发时机 |
|---|---|---|
| `item` | A1 `Item`（14 case 之一）；`turnId: string`；`itemId: string` | Engine 每次 emit item 时（B1 `tx_event`） |
| `turn_start` | `turnId / input: Vec<UserInput>` | Turn 开始（A1 `TurnStartedNotification`） |
| `turn_end` | `turnId / status: TurnStatus / error?` | Turn 完成（A1 `TurnCompletedNotification`） |
| `compaction` | `summary: string / firstKeptEntryId: string / tokensBefore: number` | Pi `CompactionEntry` 同名（types.ts:357-363） |
| `branch_summary` | `fromId: string / summary: string` | Pi `BranchSummaryEntry`（types.ts:366-372），fork 时旁注 |
| `model_change` | `provider / modelId` | Pi `ModelChangeEntry`（types.ts:351-355） |
| `leaf` | `targetId: string \| null` | **仅 fork / 回放选支时写**；普通 append 不写（Pi 规则，§4.4 解释） |

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

> 仅定义 trait + 方法签名；**不写 impl**。具体 impl 落地在 B3 完成后由实现工程师写 `crates/zhive-core/src/persistence/`。

```rust
//! crates/zhive-core/src/persistence/mod.rs（待补）

use std::sync::Arc;

/// 4 库聚合接口（D-011 §决策）。
///
/// 实现层握有 4 个 [`ConnectionPool`]（或 4 个 `Arc<Mutex<Connection>>`，
/// 选型见 §6），按需借 connection。
///
/// **跨库一致性**：本 trait **不提供** 跨库事务原语（rusqlite 不支持
/// ATTACH-level 多 DB 写事务有限）。调用者必须遵循 §7 的 "JSONL source of truth"
/// fail-strategy。
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
    async fn create_thread(&self, args: CreateThreadArgs) -> Result<ThreadId, PersistenceError>;

    /// 列出 thread（分页 + status 过滤）。**JSONL 是 source of truth**：
    /// 这里只返回索引视图（标题、created_at 等），不还原全部 item。
    async fn list_threads(&self, q: ThreadQuery) -> Result<ThreadPage, PersistenceError>;

    /// 取单 thread 元信息（不含 item / turn 历史）。
    async fn get_thread(&self, id: &ThreadId) -> Result<Option<ThreadMeta>, PersistenceError>;

    /// 新 turn 入索引（不存 turn 内 item；item 流落 JSONL）。
    async fn record_turn_start(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        started_at_ms: i64,
    ) -> Result<(), PersistenceError>;

    /// turn 结束时更新索引。
    async fn record_turn_end(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        status: TurnStatus,
        completed_at_ms: i64,
    ) -> Result<(), PersistenceError>;

    /// 软删除 thread（status -> 'archived'）。**不级联** logs / memories / goals。
    async fn archive_thread(&self, id: &ThreadId) -> Result<(), PersistenceError>;
}

// ---- LogsDb（logs.db） ----

#[async_trait::async_trait]
pub trait LogsDb: Send + Sync {
    /// 单条 log 落盘。`thread_id == None` 表示 process-level log（如 startup）。
    async fn record_log(&self, log: LogEntry) -> Result<(), PersistenceError>;

    /// 批量 log（高频路径走批写）。
    async fn record_logs(&self, logs: Vec<LogEntry>) -> Result<(), PersistenceError>;

    /// 查询 log（按 thread / level / 时间范围）。
    async fn query_logs(&self, q: LogQuery) -> Result<Vec<LogRow>, PersistenceError>;

    /// 清理：删除 ts_ms < cutoff 的 log（运维用）。
    async fn purge_logs_before(&self, cutoff_ms: i64) -> Result<u64, PersistenceError>;
}

// ---- MemoriesDb（memories.db） ----

#[async_trait::async_trait]
pub trait MemoriesDb: Send + Sync {
    /// upsert：若 id 已存在则更新 body / tags / updated_at_ms。
    async fn upsert_memory(&self, mem: Memory) -> Result<(), PersistenceError>;

    /// 按 id 取单条。
    async fn get_memory(&self, id: &MemoryId) -> Result<Option<Memory>, PersistenceError>;

    /// FTS5 search：`query` 走 sqlite MATCH 语法。
    async fn search_memories(&self, q: MemoryQuery) -> Result<Vec<Memory>, PersistenceError>;

    /// 删除单条。
    async fn delete_memory(&self, id: &MemoryId) -> Result<(), PersistenceError>;
}

// ---- GoalsDb（goals.db） ----

#[async_trait::async_trait]
pub trait GoalsDb: Send + Sync {
    /// 新增 goal。复合主键 (thread_id, goal_id) 冲突时返回 `PersistenceError::Conflict`。
    async fn add_goal(&self, goal: Goal) -> Result<(), PersistenceError>;

    /// 标记 goal 完成（status -> 'complete'）+ 更新 updated_at_ms。
    async fn mark_done(&self, thread_id: &ThreadId, goal_id: &GoalId) -> Result<(), PersistenceError>;

    /// 列 goal（按 thread / status）。
    async fn list_goals(&self, q: GoalQuery) -> Result<Vec<Goal>, PersistenceError>;

    /// 累加 tokens_used（每 turn 结束时调用）。
    async fn add_tokens_used(
        &self,
        thread_id: &ThreadId,
        goal_id: &GoalId,
        delta: u64,
    ) -> Result<(), PersistenceError>;
}

// ---- 共享类型（占位；具体字段沿 A1 / D-011） ----

#[derive(thiserror::Error, Debug)]
pub enum PersistenceError {
    #[error("sqlite error: {0}")] Sqlite(#[from] rusqlite::Error),
    #[error("connection pool exhausted")] PoolExhausted,
    #[error("not found")] NotFound,
    #[error("conflict (already exists)")] Conflict,
    #[error("schema migration failed: {0}")] Migration(String),
    #[error("io: {0}")] Io(#[from] std::io::Error),
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
| `Storage` 4-method aggregate trait | codex `StateRuntime` （`lib.rs:21,46`）—— 单一 struct 不是 trait | zhive 选 trait 是为测试 mock；codex 直接 struct 是因为 codex 不做 in-memory mock |
| `StateDb / LogsDb / MemoriesDb / GoalsDb` 4 trait | codex `MemoryStore / GoalStore`（`lib.rs:55-58`）+ `log_db` module —— **2 trait + 1 module，未对齐** | zhive 4 trait 对齐 4 库，工程感更整齐 |
| `record_log` / `query_logs` | codex `log_db::*`（`state/src/log_db.rs`） | 同语义 |
| `upsert_memory / search_memories` | codex `MemoryStore` | 同语义 |
| `add_goal / mark_done` | codex `GoalStore / GoalUpdate / GoalAccountingMode`（`lib.rs:54-57`） | zhive 砍 `GoalAccountingMode`（codex 用于 token 限额，Phase 1 不引入） |
| `forked_from_id` 字段 | codex 无对应（codex 用 `threads.rollout_path` 中文件命名传） | zhive 抄 Pi `parentSession` |
| `agent_jobs / agent_job_items` 表 | codex 有（`migrations/0014_agent_jobs.sql`） | **zhive Phase 1 拒**（CSV batch job 非范围） |
| `stage1_outputs / jobs / phase2_*` | codex `memory_migrations/0001_memories.sql` | **zhive 拒**（codex 后处理流水线，与 zhive memories 语义不同） |

---

## 6. Connection pool 选型（R-7 三方案对照 + 用户决策建议）

### 6.1 背景：rusqlite 0.40 `Connection` 不是 Send/Sync

参考 `${SQL}/src/lib.rs:437-548`，`Connection` 是 `!Send + !Sync`（内部用 `RefCell` 包裹原始指针）。多线程访问必须：

- **方案 a**：每线程 / 每 task 自己 `Connection::open(path)` —— 简单但每次 open 都重做 PRAGMA。
- **方案 b**：用 connection pool（r2d2-sqlite / deadpool-sqlite）—— 共享 idle connection 复用。
- **方案 c**：自写 `Arc<Mutex<Connection>>`（无 pool）—— 全局序列化所有 DB 操作，简单到 ~50 行。

下表三方案对照：

| 方案 | 新依赖? | 实现量 | 优点 | 缺点 | 适用场景 |
|---|---|---|---|---|---|
| **a. r2d2-sqlite** | ✅ 触发红线 1（`r2d2` + `r2d2-sqlite` 2 个新 crate） | ~30 行 setup | 同步 API（与 rusqlite 0.40 同步语义匹配）；社区主流；与 rusqlite 同 maintainer | 不与 tokio 异步亲和（要 `spawn_blocking`）；2 个新依赖 | 多线程同步代码 |
| **b. deadpool-sqlite** | ✅ 触发红线 1（`deadpool` + `deadpool-sqlite` + `tokio-rusqlite` 3 个新 crate） | ~40 行 setup | tokio-native；async API；推荐用于 tokio 项目 | 3 个新依赖；其中 `tokio-rusqlite` 封装层会包裹 `Connection` 进 spawn_blocking 后台线程 | tokio app（zhive ✓） |
| **c. 自写 mini pool** | ❌ 不触发红线 | ~80-150 行 | 0 新依赖；可控；可精确按 4 库各持 N 个 connection | 维护负担；要写池子的 acquire / release / 死链回收；测试覆盖工作量 | 强约束新依赖时 |
| **d. 无 pool（`Arc<Mutex<Connection>>`）** | ❌ 不触发红线 | ~20 行 | 0 新依赖；最简；DB 串行写避免 SQLITE_BUSY | 写并发 = 0；高频写会变瓶颈；读也被锁死（除非每 DB 一个 reader connection） | 单库少并发；MVP |

### 6.2 推荐方向（**仅建议，等用户决策**）

> 本任务**不擅自决策**，因为方案 a / b 触发 CLAUDE.md 红线 1（禁新依赖）。

**短期方向（Phase 1 起步）**：**方案 d**「`Arc<Mutex<Connection>>` × 4 库」。
- 每库 1 个 `Arc<Mutex<Connection>>`（**不是 4 个共享**，每库独立锁 → §6.3 WAL 行为）
- 0 新依赖
- 阻塞写时间 < 10ms（rusqlite + WAL 单写者吞吐），Phase 1 可接受
- 读路径未来需要并发时，每库加一个 readonly `Connection`（`OpenFlags::SQLITE_OPEN_READ_ONLY`），用 `RwLock<...>` 升级方案

**中期演进路径**：**方案 c**「自写 mini pool」，**或** 方案 b（deadpool-sqlite），等 Phase 2 实测出热点再选。

**触发"禁新依赖"红线的处理建议**：
- 若用户希望直接上 deadpool-sqlite，请在 PR 描述里走 CLAUDE.md 红线 1 流程：列依赖 + 理由 + 等批准
- 若用户优先「现在能跑」，选方案 d 起步，留 `// TODO: pool` 注释

### 6.3 多 connection 下 WAL 行为（Q3 直接回答）

rusqlite 同进程多 `Connection` 打开**同一个** DB 文件 + `journal_mode=WAL`：

- **支持**：sqlite WAL 模式允许多 reader + 1 writer 并行（这是 WAL 比 rollback journal 的核心优势）
- **同进程多 `Connection`**：每个 `Connection` 是独立的 sqlite session，互不共享 prepared cache / transaction state，但共享同一份 `*.db-wal` 文件
- **PRAGMA journal_mode 持久化**：journal_mode 是 DB 级别（写在 sqlite header），**任一 connection 改了即对所有 connection 生效**。但**每个 connection 启动后仍要单独执行 `PRAGMA journal_mode = WAL;` 以确认**（rusqlite docs / sqlite spec），否则 `BEGIN` 会用默认 rollback 路径 fallback。
- **zhive 起步建议**：每库**独立** `Arc<Mutex<Connection>>`，每个 `Connection::open` 后第一条 SQL 永远是：
  ```rust
  conn.pragma_update(None, "journal_mode", "WAL")?;
  conn.pragma_update(None, "synchronous", "NORMAL")?;  // WAL 配 NORMAL 即可，FULL 太慢
  conn.pragma_update(None, "foreign_keys", "ON")?;
  conn.pragma_update(None, "busy_timeout", 5000)?;     // 5s 超时，配 multi-reader 场景
  ```
- **每库独立 pool 还是共享 pool？**：**独立**。4 库是 4 个文件，sqlite WAL 锁是文件级；用共享 pool 是无意义的（共享池里的 connection 各自指向不同文件，不能复用）。

---

## 7. 跨库一致性策略（R-8）

### 7.1 跨库事务原子性：**不保证**

rusqlite 0.40（以及 sqlite 本身）的 `BEGIN TRANSACTION` 是单 DB 文件作用域。多 DB 想原子写有三个理论方案：

1. **sqlite ATTACH DATABASE**：把 4 个文件 ATTACH 到同一 main DB，写事务跨 ATTACH。
   - 限制：WAL 模式 + ATTACH 的写事务**仅 main DB 可写**，ATTACH 上来的库默认 read-only（[sqlite WAL doc](https://www.sqlite.org/wal.html) §10）。
   - 即使关 WAL 也会有性能 + 死锁回退路径，**不采纳**。
2. **应用层 2PC**：复杂；与 D-009 "尽量简" 矛盾，**不采纳**。
3. **JSONL source of truth + DB 异步重建**（D-011 修订正文采纳）：DB 仅是索引，原子性由 JSONL 单文件 append 保证。**zhive 选此**。

### 7.2 Fail-strategy：JSONL 先写成功，DB 失败可异步重建

**写顺序硬约定**（B1 actor 内强制）：

```text
顺序     操作                                        失败处理
1.     append JSONL（fsync 完成后才算 OK）         失败 → 整体 fail，向上抛 PersistenceError
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
) -> Result<RebuildStats, PersistenceError> {
    let mut stats = RebuildStats::default();
    for entry in std::fs::read_dir(rollouts_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if path.file_name() == Some(OsStr::new("session_index.jsonl")) {
            continue;  // 索引文件，跳过
        }

        // 1. 解析 header（第一行）
        let mut reader = BufReader::new(File::open(&path)?);
        let mut header_line = String::new();
        reader.read_line(&mut header_line)?;
        let header: SessionHeader = serde_json::from_str(&header_line)?;

        // 2. upsert thread metadata（state.db）
        storage.state().upsert_thread_from_header(&header).await?;

        // 3. 逐行重放 entry，按 entry.type 分派到 4 库
        for line in reader.lines() {
            let entry: SessionTreeEntry = serde_json::from_str(&line?)?;
            match entry.kind() {
                EntryKind::TurnStart { thread_id, turn_id, ts } =>
                    storage.state().record_turn_start(&thread_id, &turn_id, ts).await?,
                EntryKind::TurnEnd { thread_id, turn_id, status, ts } =>
                    storage.state().record_turn_end(&thread_id, &turn_id, status, ts).await?,
                EntryKind::Item { .. } => {
                    // item 本身落在 JSONL；这里不重建 item 表（zhive 无）
                    // 但如果 item 是 PreCompact / MemoryWrite，触发 memory upsert
                }
                EntryKind::Compaction { .. } |
                EntryKind::BranchSummary { .. } |
                EntryKind::ModelChange { .. } |
                EntryKind::Leaf { .. } => {
                    // 这些 entry 仅 JSONL 用，DB 无对应表
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

## 8. R-2 实测：rusqlite 0.40 + bundled + 4 DB 编译与启动数据

> **本节是 R-2 直接证据**。实测于 **2026-05-28**，使用临时 probe crate `crates/zhive-persistence-probe/`（已删除）。

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

> **R-2 触发**：cold release build **78-80s > 60s 阈值**（plan §9 R-2 阈值）。**未达 >2min 极端值**，但显著超过 60s。

**Phase 1 实际影响估算**：
- zhive-core 引入 rusqlite + bundled 后，**zhive-core 单 crate cold release** 估算 += 60-65s（probe crate 是空 main，zhive-core 已有 tokio / serde / schemars 等大依赖，故 zhive-core cold release **预估 90-150s**）
- dev profile（开发主路径）影响 += ~8-10s，可接受
- 二进制体积 **+2.5 MiB**：zhive-core 静态库会承担 ~2.5MB 的 libsqlite3 C 编译产物，最终 `zhive-cli` binary 体积估算 += 2.5MB

**未提前缓解的迹象**：
- workspace 的 `[profile.dev.package."*"] opt-level = 1` 设置会让 sqlite C 也走 opt-1，dev cold build 可能比 probe（dev=0）更慢，需进一步验证 → §10 未决项
- workspace 的 `[profile.release] strip = "symbols"` 可减少 ~7% 体积（参考 §8.2 stripped 2.5 vs unstripped 2.7）

### 8.4 缓解建议

1. **CI 配 sccache**（已部署，per memory `feedback-sccache-incremental.md`）—— sqlite C 编译产物可 cache，**第二次 CI 跑及之后约 < 5s**
2. **本机开发用 dev profile**（cold = 11s OK）+ release 仅在打包 / dist 时触发
3. **不切 `bundled-sqlcipher`**（会拉 OpenSSL，体积 +5MB / cold +30s）
4. **关闭 rusqlite 默认不需要的 feature**：当前只用 `bundled`；不开 `vtab / array / functions / hooks / chrono` 等
5. 长期：考虑 `libsqlite3-sys` 改用系统 sqlite3 `.so` 链接 —— 砍编译时间，但 Phase 1 不做（Windows / macOS 用户体验劣化）

---

## 9. 与 codex 当前实现的逐项对照

| 项 | codex（当前主线） | zhive Phase 1 | 取舍 |
|---|---|---|---|
| ORM | `sqlx` (`state/Cargo.toml`) | `rusqlite =0.40 bundled` | D-011 锁定 rusqlite，理由：cargo check -p 工作流 + 同步 API 更可控 |
| 库分离 | 4 个文件，35+2+1+1 migrations | 4 个文件，1+1+1+1 migration（终态直落） | zhive 直接抄结构但跳过 codex 演进留痕 |
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
| FTS5 memory 搜索 | 无（codex 用关键字 LIKE） | 有 | zhive 加 FTS5（rusqlite bundled 自带） |
| Connection 抽象 | sqlx Pool | trait + impl 待定（§6 三方案对照） | 等用户决策 |

---

## 10. R-2 / R-7 / R-8 触发与缓解

| 风险 | 触发 | 本 deliverable 给出的缓解 |
|---|---|---|
| **R-2**（rusqlite bundled cold build） | ✅ 触发（78-80s release，超 60s 阈值） | §8.4 缓解清单：sccache（已部署）+ dev profile 日常 + 关闭多余 feature；建议向用户确认是否接受 release 慢路径 |
| **R-7**（pool 新依赖红线） | 部分触发（方案 a/b 引新 dep） | §6 三方案对照；推荐 d 起步 + c 中期演进；**未自决，等用户拍** |
| **R-8**（跨库一致性） | ✅ 触发（4 库无原子事务） | §7 fail-strategy：JSONL source of truth + 异步重建索引；崩溃恢复伪码已落地 §7.3 |

---

## 11. 未决项

> 全部按 plan §10 回流原则待补到 D-011 / B3 实施任务。

- **TODO(开放项 B3-1)**：用户手写 memory（`kind="note" / "fact"`）是否也走 JSONL 落盘 → 当前方案接受 memories.db 损坏 = 手写丢失。**待用户决策**：是否新增 `memories.jsonl` source of truth？
- **TODO(开放项 B3-2)**：connection pool 选型 —— §6 推荐方案 d（`Arc<Mutex<Connection>>`）起步，但需用户确认（避免后续返工换 deadpool-sqlite）。
- **TODO(开放项 B3-3)**：Pi `MemoryRepo` 是否分 thread-local / 全局两类？zhive 目前 schema 用 `thread_id NULL` 表示全局，等 A5 / B2 调研最终拍。
- **TODO(开放项 B3-4)**：rusqlite **release** cold build 78-80s 超阈值；用户是否接受这个 trade-off？若要砍，需切「系统 sqlite3 链接」方案（Windows/macOS 用户体验劣化）或切「runtime download libsqlite3」（运行时依赖）。
- **TODO(开放项 B3-5)**：`zhive-core` 把 SQLite + 4 库 + rollout 全揉一起，**单 crate 编译时间** 实测可能 > 150s。是否拆 `zhive-persistence` 子 crate（违反 D-001 "7 crate 起步" 约束）？或继续单 crate 等 sccache + 增量缓解？
- **TODO(开放项 B3-6)**：cross-DB ID 跨域映射工具：`logs.thread_id ↔ state.threads.id` 软关系，建议落「ID newtype + 跨 trait 校验函数」防止 lint 漏过。
- **TODO(开放项 B3-7)**：测试矩阵。当前 trait 草图已设计，但 `MockStorage`（in-memory 测试替身）的实现策略未定：用 `:memory:` SQLite 还是用纯 `HashMap` impl？
- **TODO(开放项 B3-8)**：`workspace [profile.dev.package."*"] opt-level = 1` 对 sqlite C 编译时间影响未单测 —— §8.3 提到可能让 dev cold build 比 probe（dev=0）更慢，需 zhive-core 真接入 rusqlite 后再测一次。

---

## 12. 一句话总结（plan §10 回流摘要）

zhive Phase 1 直接落 4 库（state / logs / memories / goals）+ JSONL Leaf rollout；4 个 `0001_*.sql` 已对照 codex 同名 migration 写好；`Storage` trait + 4 子 trait + cross-DB fail-strategy（JSONL source of truth + 异步 DB 重建）已就位；R-2 实测**触发**（release cold 78-80s，>60s 阈值），二进制 +2.5MB，启动 1.5ms；R-7 不自决，**等用户在「`Arc<Mutex<Connection>>` 起步」与「上 deadpool-sqlite 引 3 个新 dep」之间拍板**。
