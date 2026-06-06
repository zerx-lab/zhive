---
topic: 文件编辑回退机制（撤销 agent 改动到上一状态）
date: 2026-06-07
status: active
---

# 文件编辑回退机制 — 调研 + 设计

> 调研方法：多智能体 workflow（12 agent，5 路深读 + 3 哲学设计 + 综合 + 3 路对抗式批判），全部基于真实代码核实，带 `file:line` 证据。

## 结论先行

1. **采用 opencode 的「独立影子 git 仓库 + 内容寻址快照」为主干。** 它是四个参考项目里唯一做到*真还原磁盘 + 跨进程重启持久 + 二进制天然支持 + 不受 diff 体积上限影响*的方案，且与 zhive「rollout 持久 source-of-truth + resume」架构同构。
2. **回退语义 = 文件 + 对话一起 rewind**（已拍板）。`/undo` 把磁盘文件**和**对话历史一起回到同一点（复用已验证的 `Submission::Fork{up_to_item}`），避免 codex 同款「磁盘回退了但 agent 上下文还以为改过」的不一致 bug。
3. **综合方案原版有 5 处经核实的承重缺陷，必须先修**（见 §4）。最危险的是把 `EngineConfig.cwd` 当 git work-tree —— 实测它只是元数据列，工具落盘走的是进程全局 `std::env::current_dir()`，照此实现会「以为还原了其实没还原」。
4. **零新 Cargo 依赖**（`tokio::process` 调系统 git）。唯一环境前提是运行时存在 git 二进制，可探测、可优雅降级。
5. **ROI 未决**：仓库 `todo.md` backlog 没有此项；用户多在 git 仓库已有 `git restore` + `/fork`。边际价值见 §8，留作产品判断。

---

## 1. 调研：各参考项目对比

| 项目 | 文件回退 | 存储后端 | 粒度 | 真还原磁盘？ | 跨重启持久？ | 触发 UX |
|---|---|---|---|---|---|---|
| **opencode** | ✅ 完整 | **独立影子 git 仓库**（`write-tree`/`checkout-index`，GIT_DIR 在 XDG data） | per-step（每个 LLM step） | ✅ | ✅ tree object 在磁盘 | `/undo` `/redo`、`<leader>u`、revert-dock 点选消息 |
| **codex** | ⚠️ **只回退对话** | rollout JSONL（对话）+ 内存 `TurnDiffTracker`（仅供展示 diff） | per-turn | ❌ **磁盘文件不动** | 对话 ✅ / 文件 ✗ | Esc 两连（无 slash） |
| **pi** | ⚠️ 仅可选扩展 | `git stash create`（扩展持有内存 Map） | per-turn | ✅（需装扩展 + 手动确认） | ❌ 内存 Map，重启即失 | `/fork` → 询问「恢复代码？」 |
| **zed** | ✅ per-hunk | 内存 `Rope`（`ActionLog.diff_base`） | per-tool-call | ✅ 写回 buffer + 存盘 | ❌ 关闭即失 | Agent Diff 面板逐 hunk Keep/Reject |
| **zhive 现状** | ❌ **无** | — | — | — | — | 无 |

### 1.1 opencode（选为主干）

- 机制：独立影子 git 仓库，`git --git-dir <shadow> --work-tree <worktree>` 操作，**不碰用户 `.git`**。`Snapshot.track()` = stage（`diff-files` 改动 + `ls-files --others` 未跟踪）后 `git write-tree` 生成 tree hash（**不创建 commit**）。回退 `git checkout <hash> -- <file>`，文件在目标 tree 不存在则删除（撤销新建）。
- 存储路径：`{XDG_DATA}/opencode/snapshot/{projectID}/{hash(worktree)}/`；tree hash 存进 session 的 `PatchPart` 消息部件。
- 时机：stream 启动前预捕获 + 每 step-start 确认 + step-finish 后 `patch()` 记录变更文件。
- 边界：尊重 `.gitignore`（`check-ignore`）；新建文件 >2MB 排除；二进制以 git blob 存储照常恢复；每小时 `gc --prune=7.days`。
- 证据：`packages/opencode/src/snapshot/index.ts:79-84`（路径）、`:278-301`（track/write-tree）、`:336-360`（全量 restore）、`:362-394`（逐文件回退 + 删除新文件）；`packages/opencode/src/session/revert.ts`（与消息历史联动）。
- 缺点：仅 git 项目可用；已 tracked 大文件无 2MB 上限保护；项目移动后 `hash(worktree)` 变 → 找不到旧快照。

### 1.2 codex（路线出局，借鉴绑定语义）

- **核心缺陷（codex 自陈）：rollback 完全不还原磁盘文件** —— `apply_patch` 写入的变更永久保留，只有 LLM 上下文被回退到某条 user message（`num_turns` 截断）。
- `TurnDiffTracker`（`old_content`/`new_content`）仅存活进程内存、仅供展示 unified diff，不持久化。进程崩溃即丢。
- 借鉴点：「文件回退 + 对话截断绑定到同一点」的一致性直觉（zhive 用 Fork 实现，见 §5）。
- 证据：`thread_rollback` 倒序回放 rollout + `drop_last_n_user_turns`；`ThreadRolledBack{num_turns}` 写 JSONL。

### 1.3 pi（不可移植为默认）

- 核心**没有**内置回退；仅 `examples/extensions/git-checkpoint.ts`（~54 行）可选扩展，在 `turn_start` 跑 `git stash create` 存内存 Map，`/fork` 时询问是否 `git stash apply`。
- 致命：stash ref 仅在内存、`agent_end` 清空、重启即失；`git stash create` 默认**不含未跟踪文件** → 新建文件回退不掉。

### 1.4 zed（内存方案，架构不同构）

- `ActionLog` 在内存以 `Rope` 持有每个 buffer 的 `diff_base`（编辑前全文），per-tool-call 捕获，`RejectAll`/逐 hunk `Reject` 写回 buffer 再存盘。
- 优点：处理用户与 agent 交错编辑（`apply_non_conflicting_edits` 把用户改并入 `diff_base`）；三态（Modified/Created/Deleted）正确回退；一级 undo。
- 致命：**全在内存，关闭即失，无跨会话持久**；无 per-message checkpoint；二进制不支持（非 text buffer 不追踪）。

### 1.5 zhive 现状（地基）

- write/edit 工具 `temp+rename` 原子写但**落盘前不备份旧内容**，也不生成 `Item::FileEdit`/`Item::Diff`（`write.rs`）。
- **可复用的就绪结构**：`Item::FileEdit{changes:Vec<FileUpdateChange>, status}`、`Item::Diff{old_text,new_text}` 数据模型已就绪（`domain.rs:626-646`）；`RolloutEntry`/`StorageWriteOp`/`EnginePhase`/`Submission` 全 `#[non_exhaustive]` 可安全扩展；JSONL rollout 为 source-of-truth + SQLite 投影；`fork.rs` 的 phase-CAS + Drop-guard + `Flush{ack}` 是现成模板；`Submission::Fork{up_to_item}` 已验证可截断对话开干净分支。
- 数据目录：`~/.local/share/zhive`（`boot.rs::data_dir()`）。
- 迁移规则：禁改已应用迁移（checksum mismatch 毁库），只走新迁移文件。

---

## 2. 选型理由（为何 git-shadow 主干）

- **vs codex**：codex 不还原磁盘，与「撤销 agent 文件改动」的产品目标相悖，直接出局。
- **vs content-copy/SQLite（存 old_text 全文）**：影子 git **内容寻址**（相同 blob 只存一份，rollout 只存 40 字符 tree hash → **不膨胀**），二进制天然支持，**不依赖 FileDiff 持久化**（即使 zhive 的 256KiB `FileDiff` cap 丢了 diff，回退仍 100% 正确），且覆盖 bash 等任意工具的文件改动。content-copy 全量副本会让 rollout/DB 随大文件线性膨胀。
- **vs patch-chain（复用 rollout 内 Diff 做反向 patch）**：最省存储但有能力空洞 —— 大文件（>256KiB）被 cap 丢弃后无法回退，且依赖「compaction 后 rollout 行物理不删」不变量，未来引入 rollout GC 会断链。git-shadow 从 tree object 取内容，与 cap/compaction/GC 解耦。
- **vs zed/pi（内存）**：影子 tree object 持久在磁盘 → **跨 resume 回退仍可用**，与 zhive 持久化哲学同构。
- **借鉴融合**：嫁接 content-copy 的「外部编辑冲突检测」（用 `git status` 实现，更自然）+ codex 的「文件+对话回退到同一点」（复用 Fork）。

---

## 3. 推荐设计（三层结构）

落在 zhive 现有 `actor + rollout(JSONL source of truth) + SQLite 投影 + phase-CAS` 架构上。

### (1) 快照层 — `zhive-core/src/snapshot/mod.rs`（新模块）
`ShadowRepo`：对一个 workspace 维护独立 `GIT_DIR`（`~/.local/share/zhive/shadow/<sanitised(root)>/`，不碰用户 `.git`）。
- `track()` = `git add -A` + `git write-tree`（**不创建 commit** → 轻量、无需 git identity、不污染任何 reflog），产 40 字符 tree hash。
- 所有 git 调用统一附加隔离参数：`-c core.hooksPath=/dev/null -c filter.lfs.smudge=cat -c filter.lfs.clean=cat -c filter.lfs.process=`（见 §4 LFS 修正）。
- 全部 `tokio::process` + `Result`/`thiserror` + `?`，**禁 unwrap**，仔细处理退出码/stderr/超时。

### (2) 关联层 — rollout + SQLite
- 新 `RolloutEntry::Snapshot{thread_id, turn_id, tree}` 持久化进 JSONL（source of truth），投影到新表 `turn_snapshots`（迁移 `0003`）。turn 级粒度，与 `TurnEnded` fsync save point、resume 重放对齐。
- tree hash 极小（40 字符）→ rollout 不膨胀（相对 content-copy 的决定性优势）。

### (3) 回退层 — `Submission::Restore`
`/undo` → TUI warn 确认 overlay（**回退前预览将动的文件清单**）→ `engine/restore` RPC → `Submission::Restore` → 引擎在 `EnginePhase::Restore`（CAS + Drop-guard，照搬 `fork.rs`）下执行：
1. 阶段闸门 + 活跃子 turn 检查（见 §4 硬伤 3）。
2. `Flush{ack}` 等最新 snapshot 落盘。
3. 写回退 journal `RestoreStarted`（见 §4 原子性）。
4. **逐文件**还原本 turn 改动集（`git checkout <tree> -- <file>`）+ 删除本 turn 新建文件 + 冲突检测。
5. **复用 `Submission::Fork{up_to_item}` 截断对话到同一点**（文件 + 对话一起 rewind，见 §5）。
6. 写 `RestoreCommitted` + Flush fsync。
7. 广播 `EngineEvent::Restored{reverted, skipped, out_of_tree}` → TUI flash。
8. git 不可用 → 明确提示「undo unavailable」，**绝不静默失败**。

---

## 4. 关键修正（评审经真实代码核实的承重缺陷）

> 这是本设计相对原始综合方案的核心增量。**不修这几条会直接导致数据损坏或运行时必败。**

### 🔴 硬伤 1：work-tree 锚定错位（最危险）
综合方案称「`EngineConfig.cwd` 已是 workspace 根可直接当 work-tree」—— **核实为假**：`EngineConfig.cwd`（`engine.rs:234-238`）只是元数据列（默认 `PathBuf::from(".")`，用于按项目列会话），而 write/edit 工具落盘走 `resolve_path → std::env::current_dir()`（`builtin.rs:294-304`，进程全局 cwd），两者只在 CLI 启动时巧合相等，无代码把进程 cwd 钉死。漂移时 git `--work-tree` 拍错目录 → `/undo` flash「restored N files」但磁盘没变 = 静默数据信任崩塌。

**修法：** 引擎建立 canonical `workspace_root`（构造时 `canonicalize` 一次），**让工具 `resolve_path` 基于该 session root 解析而非活的进程 cwd**。这处小而有原则的改动顺带消除 zhive 现存的「多工具并发进程 cwd 竞态」既有 bug。git work-tree 用这个 root。（综合方案「零工具侵入」卖点建立在错误前提上，诚实最优解是这处受控改动。）

### 🔴 硬伤 2：工作区外文件无法回退且无告警
write/edit 接受任意绝对路径（无 jail，`write.rs:94/244`），bash 接受任意 cwd（`bash.rs:176-209`）。影子 git 只覆盖 work-tree 内路径，外部改动既不进快照也不被回退，而 `/undo` 仍 flash 纯成功 → 误导。

**修法：** dispatch 层收集本 turn 工具真实写入的 dest 路径集合；落在 root 外的进 `RestoreReply.out_of_tree[]`，回退时明确报告 `not reverted (outside workspace): ...`。flash 文案改 `restored N files, M skipped (outside workspace)`。**绝不在有外部改动时显示纯成功。** V2 可对 out-of-tree 加 content-copy 兜底。

### 🔴 硬伤 3：子 agent 并发 turn 共享单一影子 index → `git add` 竞态
`subagent_spawn/mod.rs:243` `tokio::spawn(run_child_turn_and_deliver)`，子 turn 走同一 `run_turn`（即 `track()` 注入点，`:439`），且 `mod.rs:85-87` 明确「派发子 agent 时引擎**不改全局相位**」。后果：① 父 + N 个子 turn 并发 `git add -A` 写同一 index → 抢 `index.lock`，损坏或失败；② 回退闸门 `Idle→Restore` 在子 agent 在飞时形同虚设（全局相位仍 Idle），`/undo` 会对子 agent 正在写的文件 `checkout` → 撕裂磁盘。

**修法：** ① **只在顶层 user turn 做 `track()`**（判别：`PermissionScope::default_turn_scope()` vs narrowed，或 `handle.parent_thread_id`）；子 turn 跳过 —— turn 级全量快照会在下一顶层 turn 把子 agent 改动纳入新基线，覆盖不丢。② `ShadowRepo` 所有 git 操作加 per-repo `tokio::sync::Mutex` 串行化（也互斥 track/restore）。③ 回退前检查有无活跃子 turn，有则 `RestoreError::EngineBusy`。

### 🔴 硬伤 4：相位转移表未含 `Idle→Restore` → 每次回退必报错
`inner.rs:848` `try_set_phase_atomic` 先查 `phase.rs:29-38` 的 `allows_transition` 白名单，其中**没有任何到 `Restore` 的边**。直接「照搬 fork.rs」会让每次 `/undo` 拿 `IllegalTransition` → 功能 100% 不可用。

**修法：** `phase.rs` 的 `matches!` 加 `(Idle, … | Restore)` 与 `(Restore, Idle)` 两条边，并补穷尽测试 `idle_can_start_restore` / `restore_returns_to_idle`（对齐 BranchSummary 测试形态 `phase.rs:44-81`）。

### 🟡 其余必修（MVP 内，非进阶）
- **回退非原子 → 真 journal**：执行**前**写 `RolloutEntry::RestoreStarted{target_tree, planned_changes, planned_deletes}` + Flush ack，完成后写 `RestoreCommitted`。resume 见 Started 无 Committed → 幂等重放（`checkout` 幂等、`remove_file` 忽略 NotFound）。（综合方案把意图标记放在执行后，顺序反了。）
- **逐文件还原**，不用 `checkout-index -a -f`：全量刷盘会把用户后来手改的、agent 从未碰过的文件也还原（附带损害）。走 opencode 逐文件路线 `git checkout <tree> -- <file>`，只动「本 turn 改动集」（tree_before vs tree_after diff）。
- **LFS/clean-smudge filter 隔离**：不加 `-c filter.lfs.*=cat` 则 LFS 用户处文件被还原成指针文本 = 数据损坏 → **MVP 必修**，非进阶。
- **Snapshot 耐久窗口**：`tree_before` 必须在本 turn 第一次工具写盘**之前**确保已 enqueue 且不停留在易失 BufWriter（否则 turn 跑一半崩溃，磁盘改动已可见但快照丢失 = `/undo` 最需要的场景失效）。
- **目录键不用 `DefaultHasher`**：std `DefaultHasher` 不保证跨 Rust 版本/平台稳定 → 换 toolchain 找不到旧影子仓库。用 `sanitise_thread_id` 同款确定性编码作用于 canonical root。
- **resume 承重点**：`resume.rs:475` 有 catch-all `_ => {}`，正确忽略 Snapshot/Restored，**别改**；真正要加 arm 的是 `rebuild_state_from_entries`（`writer.rs:843`，穷尽 match，漏写不编译 → 天然安全）。
- **空 turn 不写快照行**：纯对话 turn 的 `tree_before == 上一 tree`，否则 picker 一串「回退到这里（0 文件改动）」。`/undo` 锚定「最近一个**真改了文件**的检查点」。
- **symlink**：删除新建文件若是 symlink，用 `remove_file`（只删 link 不跟随），需专门测试。

---

## 5. 回退语义：文件 + 对话一起 rewind（已拍板）

`/undo` 不只还原磁盘，还把对话历史截断到同一点：
- **复用 `Submission::Fork{up_to_item}`**（`fork.rs:91` 已验证的 ForkHeader→items→SetLeaf→Flush 耐久顺序），开干净分支。
- 好处：① 杜绝 codex 同款「磁盘回退了但 agent 上下文还以为改过、下一 turn edit 字符串匹配失败」的不一致 bug；② 对齐 claude-code `/rewind`；③ fork 回源 thread 天然提供 **redo**。
- 文件回退（真写）+ 对话回退（fork）在一个 `Submission::Restore` 内原子编排返回。
- 注意与 compaction 历史操作协调，避免双重截断；影子仓库按 cwd 跨 thread 共享，fork 出的新 thread 自动可见同一批 snapshot。

---

## 6. 改动清单（全部零新依赖、向后兼容）

| 层 | 文件 | 改动 |
|---|---|---|
| **新模块** | `zhive-core/src/snapshot/mod.rs` | `ShadowRepo`：open_or_init / track / restore（逐文件）/ 降级探测 / 隔离参数；写前必读 `ms-rust` skill（新 public API + error 类型）；无 unsafe |
| **proto** | `hook.rs` | `EnginePhase` 加 `Restore` + **`phase.rs` 合法表 + 穷尽测试** |
| | `domain.rs` | `EngineEvent::Restored{reverted, skipped, out_of_tree}`；`Item::FileEdit`/`Diff` **零改动** |
| | methods | `METHOD_RESTORE="engine/restore"` 常量 |
| **rollout** | `persistence/rollout.rs` | 加 `Snapshot` / `RestoreStarted` / `RestoreCommitted` 变体；照抄 Compaction 单向兼容 doc（`rollout.rs:100-106`） |
| **writer** | `persistence/writer.rs` | `StorageWriteOp` 加分支；**`rebuild_state_from_entries` 加 arm** |
| **sqlite** | `migrations/state/0003_turn_snapshots.sql` | 新建表（禁改 0001/0002）；`state_db.rs` 加读写 |
| **engine** | `engine.rs`/`turn.rs`/`submission.rs`/`inner.rs` | `workspace_root` 字段；顶层 turn 头 track + 耐久 enqueue；`Submission::Restore` + 阶段 CAS + Drop-guard + Flush{ack} + Fork 编排（照搬 `fork.rs`） |
| **tools** | `tools/builtin.rs` | `resolve_path` 基于 session root；dispatch 收集写入 dest 集合 |
| **TUI** | `app.rs`/`overlays.rs`/`rpc.rs` | `/undo` slash + `Action::Restore` + 复用 `overlays.rs:227` warn 确认 overlay（回退前预览文件清单） |
| **ACP** | `zhive-bridge-acp` | `availableCommands` 加 undo（复用已修的延迟首发，避 Zed 时序竞争） |

`turn_snapshots` 表：
```sql
CREATE TABLE turn_snapshots (
  thread_id  TEXT NOT NULL,
  turn_id    TEXT NOT NULL,
  tree       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, turn_id)
);
CREATE INDEX idx_turn_snapshots_thread ON turn_snapshots (thread_id, created_at);
```

---

## 7. MVP / V2 边界

**真 MVP（一个完整可用闭环）：**
- `ShadowRepo`（track/restore/降级）+ git/LFS/hooks 隔离 + 顶层-turn-only + Mutex 串行化
- 顶层 turn **dirty 快照**持久化（空 turn 不写）
- 单一 **`/undo`**：回退「最近一个真改了文件的检查点」+ 回退前 warn overlay 预览确认
- **逐文件还原** + 冲突检测（用户手改文件 skip+告警，`--force` 才覆盖）+ out-of-tree 诚实报告
- **文件 + 对话一起 rewind**（复用 `Submission::Fork`，已拍板）
- 回退 journal（RestoreStarted/Committed）+ resume 续做
- git 不可用 / storage=None / 非 git 项目 → 明确降级，绝不静默失败

**V2：**
- **turn-picker overlay**（成本被低估：TUI 已有 `render_select_list`（`overlays.rs:64`），session/skill/model picker 都复用，只是再加一个 Overlay 变体 + 列表查询 —— 这才是对标 `/rewind` 的点选体验，应优先）
- 显式 `/redo`/`unrestore`（MVP 已由 Fork 回源 thread 隐式提供 redo 能力）
- out-of-tree content-copy 兜底、影子仓库 GC（`gc --prune`）、tree_after 精确改动集

> **删掉**综合方案 MVP 里的 `/restore <turn_id>`：`TurnId` 是不透明字符串（`domain.rs:81`，形如 `t/0`），用户无从知晓也无法手打 = 永远没人能用的表面功能。点选能力整体打包进 V2 picker。

---

## 8. 风险与 ROI

**固有风险（opencode 生产已验证可接受）：** 依赖系统 git 二进制（可探测降级）；每顶层 turn 一次 `git add -A` 子进程开销（大仓库可能秒级，需 await 完成才放行工具，且 track 必须**先于**工具落盘）；影子仓库随已 tracked 大文件增长（需 GC）；回退是破坏性覆盖（确认 UX + 冲突检测兜底）；与 IDE 未保存 buffer 的冲突（checkout 直改磁盘，需文档提示「关编辑器或重载」，ACP 侧 MVP 可暂不暴露或补 buffer-reload 协议）。

**ROI（留作产品判断）：** 仓库 `todo.md` 的 P0/P1 backlog **没有**「文件回退」一项。zhive 用户多在 git 仓库、已有 `git restore` + `/fork`。内置文件回退的边际价值：① **非 git workspace 也兜底**；② **文件 + 对话一键回到同一点**（`git restore` 给不了，是 claude-code `/rewind` 的真正价值）；③ **无需用户懂 git**；④ 覆盖 bash 改的文件。是否撑得起这个体量，是产品判断。

---

## Sources

- opencode：`packages/opencode/src/snapshot/index.ts`、`src/session/revert.ts`、`src/session/processor.ts`（影子 git track/patch/restore/revert）
- codex：`thread_rollback` / `TurnDiffTracker` / `ThreadRolledBack`（只回退对话）
- pi：`examples/extensions/git-checkpoint.ts`（可选 stash 扩展）
- zed：`crates/action_log`（内存 `Rope` diff_base，逐 hunk reject）
- zhive：`crates/zhive-core/src/{tools/builtin/write.rs, engine/{turn.rs,fork.rs,inner.rs,submission.rs}, persistence/{rollout.rs,writer.rs,state_db.rs}, state/phase.rs}`、`crates/zhive-proto/src/{domain.rs,hook.rs}`
- 相关决策/陷阱：`research/99-decisions/`（D-011 best-effort 投影）、记忆 `feedback-persistence-pitfalls`、`project-edit-diff-toolcall-content`
