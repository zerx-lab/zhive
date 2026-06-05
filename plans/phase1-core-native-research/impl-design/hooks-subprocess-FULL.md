# 【全量】Subprocess hook 执行体协议 — HookExecutor::Subprocess 真正 spawn 子进程（JSON stdin → JSON stdout + exit-code 语义 + timeout/cancel/隔离）

## currentState
子进程基建已存在（关键复用锚点）：`crates/zhive-core/src/tools/builtin/bash.rs:159-209` 已有生产级 `tokio::process::Command` + `kill_on_drop(true)`(bash.rs:161) + `Stdio::piped()`(165-166) + `child.wait_with_output()` pin + `tokio::select!{ wait | sleep(timeout) | cancel.cancelled() }`(186-208)。

依赖现状：`crates/zhive-core/Cargo.toml:31` `tokio = { workspace=true, features=["io-std","process"] }` —— **process feature 已启用**，无需改 Cargo.toml。`tokio::time::timeout` 走 workspace time feature（root Cargo.toml:47 已含 "time"）。chrono **不是** zhive 依赖（验证：root+core Cargo.toml grep 空）—— 故不能学 codex 用 chrono 打时间戳，时序字段用 `std::time::Instant`/`Duration`。

HookEvent/HookOutput wire 已就绪：`crates/zhive-proto/src/hook.rs:175-214` HookEvent 内部 tag `hook_event_name`，每 variant flatten HookEventBase（hook.rs:127-150，含 sessionId/cwd/registeredBy/transcriptPath/permissionMode/agentId）。`crates/zhive-proto/src/permission.rs:557-610` HookOutput（continue/async/systemMessage/hookSpecificOutput），HookSpecificOutput::PreToolUse{permission_decision, permission_decision_reason, updated_input}（permission.rs:586-598）—— **这正是 Claude Code stdout JSON 形态，可直接 serde 反序列化子进程 stdout**。

## harnessRef
codex(Rust) 有专用 `hooks` crate，是 subprocess command hook 的完整 Rust 实现，与 zhive 几乎 1:1 可映射：

1. 子进程 spawn + JSON stdin + timeout + 隔离（核心照抄对象）：`~/Desktop/code/github/codex/codex-rs/hooks/src/engine/command_runner.rs:24-101` `run_command(shell,handler,input_json,cwd) -> CommandRunResult{exit_code:Option<i32>, stdout, stderr, error:Option<String>}`。要点：line 33-39 `build_command` 后设 `current_dir(cwd).stdin(piped).stdout(piped).stderr(piped).kill_on_drop(true)`；line 41-54 spawn 失败 → error 字段（不 panic）；line 56-69 `child.stdin.take().write_all(input_json.as_bytes()).await`，写失败 → `child.kill().await` + error；line 71-100 `timeout(Duration::from_secs(timeout_sec), child.wait_with_output()).await` 三分支：Ok(Ok(output))→收集 exit_code/stdout/stderr、Ok(Err)→IO error、Err(_elapsed)→`"hook timed out after {n}s"`。**映射到 zhive：把 `kill_on_drop` + `wait_with_output` + `timeout` 这套直接对齐 zhive 自己 bash.rs:159-209 的写法**（zhive 用 select!+sleep，codex 用 timeout()，二者等价；建议 zhive 沿用 bash.rs 的 select! 风格以便同时接 CancellationToken）。

2. exit-code 语义（决定 block 与否的核心规则）：`~/Desktop/code/github/codex/codex-rs/hooks/src/events/pre_tool_use.rs:200-286` `parse_completed`。规则：(a) `run_result.error` 有值 → status=Failed、记 error、**不 block**（非阻塞错误，对齐 CC exit!=0,!=2）；(b) `exit_code==Some(0)` → stdout 空则无意见；stdout 是合法 JSON 则 `output_parser::parse_pre_tool_use` 解析 permission_decision/updated_input/block_reason；stdout 像 JSON 但解析失败 → Failed；(c) `exit_code==Some(2)` → 用 **stderr** 作为 block_reason（line 254-269：stderr 非空→Blocked+should_block；空→Failed "exited with code 2 but did not write a blocking reason to stderr"）；(d) `Some(其它)` → Failed `"hook exited with code {n}"`（非阻塞）；(e) `None`（无退出码=被信号杀）→ Failed。

3. stdout JSON → typed 解析：`~/Desktop/code/github/codex/codex-rs/hooks/src/engine/output_parser.rs:120-181`（parse_pre_tool_use）+ `:337-350` `parse_json`（先 trim、空→None、`serde_json::Value` 校验是 object、再 from_value，**任何失败返回 None 不 panic**）+ `:352-355` `looks_like_json`（trim_start 后 starts_with '{' or '['，用来区分「纯文本输出」vs「想发 JSON 但发错了」）。

4. executor 配置结构：`~/Desktop/code/github/codex/codex-rs/hooks/src/engine/mod.rs:35-52` `CommandShell{program,args}` + `ConfiguredHandler{event_name,matcher,command:String,timeout_sec:u64,env:HashMap<String,String>,source_path,...}` —— 即 program+args+env+timeout+cwd 五要素，正是 zhive SubprocessSpec 要装的字段。`build_command`（command_runner.rs:103-135）：shell.program 空则走默认 shell（`/bin/sh -lc` non-windows，command_runner.rs:128-134），非空则 `Command::new(program).args(shell.args).arg(command)`。

5. 多 hook 隔离/排序：`~/Desktop/code/github/codex/codex-rs/hooks/src/engine/dispatcher.rs:89-116` `execute_handlers` 用 `FuturesUnordered` 并发跑、按 completion_order 标记、再 `sort_by_key(configured_order)` 恢复声明序返回 —— zhive 因红线 11 mutate 链必须**串行**（B5 §3.3 决策，与 CC 并行分歧已记 decision-diffs），故 zhive 不抄并发，沿用现 dispatch_inner 串行 for-loop，但每个 subprocess hook 子进程崩溃天然隔离（独立进程）。

6. HookFn(进程内)蓝本：`~/Desktop/code/github/codex/codex-rs/hooks/src/types.rs:12-30` `HookFn = Arc<dyn Fn(&HookPayload)->BoxFuture<HookResult>>` + `HookResult{Success, FailedContinue, FailedAbort}` —— 印证「子进程结果三态（继续/阻塞/中止）」的抽象，zhive 复用现有 HookOutput 即可承载。

Claude Code 官方 command hook 协议（已 WebFetch 验证，作为 wire 权威）：stdin 喂 JSON（含 session_id/transcript_path/cwd/permission_mode/hook_event_name + 事件特定字段如 tool_name/tool_input）；exit 0 → 解析 stdout JSON（continue/decision/reason/hookSpecificOutput.permissionDecision allow|deny|ask|defer/systemMessage/suppressOutput）；exit 2 → 阻塞，stderr 反馈给 agent，stdout 忽略；其它 exit → 非阻塞错误继续；PostToolUse/PostToolUseFailure 即使 exit 2 也不能 block（只 show stderr）；timeout 默认 command=600s（UserPromptSubmit 降 30s）；env 注入 CLAUDE_PROJECT_DIR 等；cwd = 当前工作目录。

pi(TS) 无独立 subprocess command hook（hooks 是进程内 TS handler，`~/Desktop/code/github/pi/packages/agent/docs/hooks.md` 全是 in-process HookHandler+AbortSignal），故 subprocess 协议**以 codex + CC 文档为准**，pi 仅提供 signal/串行/queue 回滚的进程内蓝本（与本特性正交）。

## approach
全量目标：把 HookExecutor 双轨从「Subprocess 占位 skip」升级为「Subprocess 真正 spawn 子进程跑完整 JSON 协议」。整体设计 = 在基线 hooks-enhance.md 的 (c) 双轨 enum 基础上，把 Subprocess variant 的执行路径实装为一个独立模块 `hooks/subprocess.rs`，dispatch_inner 命中 Subprocess 时调它。

== 拓扑 ==
HookHost.dispatch_inner 串行 for-loop（mod.rs:291）→ 每个 registration 按 executor 分派：
- InProcess(Arc<dyn HookFn>) → 现有 catch_unwind 路径（零改动）。
- Subprocess(Arc<SubprocessSpec>) → `subprocess::run_subprocess_hook(spec, event, timeout, cancel).await -> Result<Option<HookOutput>, SubprocessHookError>`。

== 子进程协议（对齐 codex command_runner.rs + CC 文档）==
时序：
1. 序列化 stdin：`let input = serde_json::to_string(event)?`（HookEvent 已带 hook_event_name + flatten base，wire 即 CC stdin 形态，hook.rs:175）。失败 → 记 warn、跳过（不 block）。
2. 构造命令：`SubprocessSpec{ program:String, args:Vec<String>, env:Vec<(String,String)>, cwd:Option<PathBuf>, shell:bool }`。若 shell=true 走 `/bin/sh -c <program>`（沿用 bash.rs:159 模式 + codex command_runner.rs:128 默认 shell）；shell=false 走 `Command::new(program).args(args)`（exec 形态，CC args 模式）。`cmd.envs(env)`、`cmd.current_dir(cwd.unwrap_or(event.base.cwd))`。
3. spawn 隔离：`cmd.kill_on_drop(true).stdin(piped).stdout(piped).stderr(piped)`（照抄 bash.rs:161-166 + codex command_runner.rs:36-39）。`cmd.spawn()` 失败 → `SubprocessHookError::Spawn` 转 warn 跳过（一个 hook 起不来不连累 engine，进程隔离第一层）。
4. 喂 stdin：`child.stdin.take()` → `write_all(input.as_bytes()).await` → `shutdown().await`（关 stdin 给子进程 EOF，否则读 stdin 的脚本卡死）。写失败 → `child.kill().await` + warn 跳过（codex command_runner.rs:56-69）。
5. 等结果 + timeout + cancel（沿用 bash.rs:183-208 的 pin+select!，比 codex 多接 CancellationToken）：
   ```
   let wait_fut = child.wait_with_output(); tokio::pin!(wait_fut);
   tokio::select! {
     r = &mut wait_fut => 解析 output,
     () = tokio::time::sleep(effective_timeout) => { warn timeout; drop(wait_fut)→kill_on_drop 杀子进程; 返回 Ok(None)=无意见 }
     () = cancel.cancelled() => { warn cancelled; 返回 Err(Cancelled) 短路整 dispatch }
   }
   ```
6. exit-code 语义（照抄 codex pre_tool_use.rs:200-286，按 event 类型分流）：
   - exit==0：stdout trim 空 → None（无意见）；非空且 `looks_like_json` → `serde_json::from_str::<HookOutput>(stdout)` 成功 → Some(output)；解析失败 → warn "invalid hook JSON"、None（非阻塞）。
   - exit==2（仅对可 block 的 event：PreToolUse/UserPromptSubmit/Stop/SubagentStop/PreCompact/PermissionRequest 有效；PostToolUse/PostToolUseFailure 按 CC 不可 block）→ 合成 block 型 HookOutput：对 PreToolUse 造 `HookOutput{ hook_specific_output: Some(HookSpecificOutput::PreToolUse{ permission_decision: Deny, permission_decision_reason: Some(stderr trim), updated_input: None }), ..default }`；stderr 空 → warn "exit 2 without stderr reason"、当非阻塞错误（None）。
   - exit==其它非0 → warn "hook exited with code {n}"、None（非阻塞，对齐 CC exit 1）。
   - exit==None（被信号杀/异常）→ warn、None。

== 全量 vs 基线如何合并 ==
基线 hooks-enhance.md (a)Unknown/(b)timeout/(d)signal 三件**保持不变全做**；(c) 从「Subprocess skip+warn 占位」**升级为本设计的真实执行体**。复用基线已规划的 HookExecutor enum + HookRegistration.executor 字段 + register 旧签名包 InProcess 保零破坏；新增 `register_subprocess_hook` 公开 API 让测试/未来 manifest loader 注册 Subprocess。timeout 字段（基线 b）与 subprocess 协议共用：Subprocess 的 effective_timeout = registration.timeout.unwrap_or(默认 600s，对齐 CC command 默认）。signal（基线 d）的 CancellationToken 直接透传给 run_subprocess_hook 的 select! cancel 分支。

== 为什么这样 ==
- 复用 bash.rs 已验证的子进程模式 → 零新依赖、风格一致、`kill_on_drop` 进程隔离已被生产验证。
- 复用 HookEvent/HookOutput 现有 serde wire → stdin/stdout 协议天然就是 CC 形态，无需新 wire 类型。
- 复用 SchemaCache（validator.rs）→ subprocess 返回 updated_input 时，红线 11 重验证走 tool_dispatch/mod.rs:464 现有路径，无需在 subprocess 模块重复校验（updated_input 经 HookOutput 折叠后由 tool_dispatch 统一 revalidate）。
- 进程隔离三层：spawn 失败 / IO 失败 / timeout 各自降级为「跳过该 hook，dispatch 继续」，只有 cancel 短路；子进程 panic/crash 是独立进程，根本不可能污染 engine runtime（比 in-process catch_unwind 更强隔离）。

## files

- `crates/zhive-core/src/hooks/mod.rs` — (1) 新增 `pub enum HookExecutor { InProcess(Arc<dyn HookFn>), Subprocess(Arc<SubprocessSpec>) }`（加 doc+小心 clone_on_ref_ptr lint：用 Arc::clone）。(2) HookRegistration.callback:Arc<dyn HookFn> 改为 executor:HookExecutor + 新增 timeout:Option<Duration>（基线 b）。(3) register() 旧签名内部包 `HookExecutor::InProcess(callback)` + timeout=None，保零破坏（现有 6 处 register 调用 + 测试桩全不动）。(4) 新增 `pub fn register_subprocess_hook(self:&Arc<Self>, registered_by, filter, priority, timeout:Option<Duration>, spec:SubprocessSpec) -> Result<ExtensionScope,HookHostError>`（复用红线10校验+排序插入逻辑，封 HookExecutor::Subprocess）。(5) dispatch_inner 快照改为 `Vec<(RegistrationId, HookExecutor, Option<Duration>)>`（executor clone 廉价：Arc 内部）；for-loop 按 executor match：InProcess 走现有 catch_unwind+（基线b）timeout 包裹；Subprocess 调 `subprocess::run_subprocess_hook(&spec, event, timeout.unwrap_or(DEFAULT_SUBPROCESS_TIMEOUT), &cancel).await`，Ok(Some)→push、Ok(None)→skip、Err(Cancelled)→break（短路）、Err(其它)→warn+continue。(6) dispatch_with_signal 入口（基线d）透传 cancel 给 subprocess。(7) `mod subprocess;` 声明 + re-export SubprocessSpec/SubprocessHookError。(8) hook_event_name/hook_session_id 已有 Unknown _ 兜底，不动。
- `crates/zhive-core/src/hooks/subprocess.rs` — 新建（独立 <300 行，避免 mod.rs 超 600 行）。内容：`pub struct SubprocessSpec{ pub program:String, pub args:Vec<String>, pub env:Vec<(String,String)>, pub cwd:Option<std::path::PathBuf>, pub shell:bool }`（doc+example）；`#[derive(Debug,Error)] pub enum SubprocessHookError{ Cancelled, Serialize(serde_json::Error) }`（注意：spawn/io/timeout 不进 Error 而是降级为 Ok(None)，只有 cancel 短路与序列化失败需上抛/记录）；`pub(crate) async fn run_subprocess_hook(spec, event:&HookEvent, timeout:Duration, cancel:&CancellationToken) -> Result<Option<HookOutput>, SubprocessHookError>` 实装上述时序 6 步；私有 `build_command(spec)->tokio::process::Command`（shell vs exec 形态，沿用 bash.rs:159 + codex command_runner.rs:103-135）；私有 `interpret_exit(event, exit_code:Option<i32>, stdout:&str, stderr:&str)->Option<HookOutput>`（照抄 codex pre_tool_use.rs:200-286 + output_parser.rs parse_json/looks_like_json 语义，按 event 是否可 block 分流）；私有 `looks_like_json`/`parse_hook_output`。`#[cfg(test)]` 用 echo/cat/sh -c 子进程做 round-trip。
- `crates/zhive-proto/src/hook.rs` — (基线a，本特性沿用不扩) 加 Unknown{name,payload} variant + 手写 Deserialize。本 subprocess 特性**不改 proto**：stdin 直接 serde HookEvent、stdout 直接 serde HookOutput，复用现有 wire。仅需确认 HookEvent Serialize 对所有 variant 正确（已验证 hook.rs:639-695 round-trip 测试绿）。
- `crates/zhive-core/src/engine/tool_dispatch/mod.rs` — 零改动。subprocess hook 返回的 HookOutput 经 dispatch 折叠后走现有 mod.rs:433-485 折叠 + mod.rs:464 红线11 revalidate 路径，统一处理（subprocess 与 in-process 的 HookOutput 同型，下游无感知差异）。
- `crates/zhive-core/Cargo.toml` — 零改动。tokio process feature 已在 line 31。无新依赖。

## newTypes
- crates/zhive-core/src/hooks/mod.rs: `pub enum HookExecutor { InProcess(std::sync::Arc<dyn HookFn>), Subprocess(std::sync::Arc<subprocess::SubprocessSpec>) }`（doc comment + 说明双轨语义）
- crates/zhive-core/src/hooks/subprocess.rs: `pub struct SubprocessSpec { pub program: String, pub args: Vec<String>, pub env: Vec<(String, String)>, pub cwd: Option<std::path::PathBuf>, pub shell: bool }`（doc + doctest 构造 echo spec）
- crates/zhive-core/src/hooks/subprocess.rs: `#[derive(Debug, thiserror::Error)] #[non_exhaustive] pub enum SubprocessHookError { #[error("hook subprocess dispatch cancelled")] Cancelled, #[error("failed to serialize hook event for subprocess stdin: {0}")] Serialize(#[from] serde_json::Error) }`
- crates/zhive-core/src/hooks/subprocess.rs: `pub(crate) async fn run_subprocess_hook(spec: &SubprocessSpec, event: &zhive_proto::hook::HookEvent, timeout: std::time::Duration, cancel: &tokio_util::sync::CancellationToken) -> Result<Option<zhive_proto::permission::HookOutput>, SubprocessHookError>`
- crates/zhive-core/src/hooks/subprocess.rs: `const DEFAULT_SUBPROCESS_TIMEOUT: std::time::Duration = Duration::from_secs(600);`（对齐 CC command hook 默认）
- crates/zhive-core/src/hooks/mod.rs: `HookRegistration.executor: HookExecutor`（替换 callback 字段）+ `HookRegistration.timeout: Option<std::time::Duration>`（基线 b）
- crates/zhive-core/src/hooks/mod.rs: `pub fn HookHost::register_subprocess_hook(self: &Arc<Self>, registered_by: ExtensionRef, filter: HookFilter, priority: i32, timeout: Option<Duration>, spec: subprocess::SubprocessSpec) -> Result<ExtensionScope, HookHostError>`（doc + doctest：注册 echo 子进程 hook）
- crates/zhive-core/src/hooks/mod.rs: 私有 `fn event_can_block(event: &HookEvent) -> bool`（PostToolUse/PostToolUseFailure → false，对齐 CC exit-2 不可 block；其余 → true），供 interpret_exit 决定 exit==2 是否合成 block 型 HookOutput

## redlineImpact
无新 crate 依赖、无新 feature、无 unsafe、无非测试 unwrap/expect：

- 依赖：`tokio::process::{Command,Child}` —— process feature 已在 crates/zhive-core/Cargo.toml:31，**不改 Cargo.toml**。`tokio::time::timeout`/`tokio::time::sleep` —— time feature 已在 root Cargo.toml:47。`tokio::io::AsyncWriteExt`（write_all/shutdown）—— io-util 已在 root Cargo.toml:47。`tokio_util::sync::CancellationToken` —— tokio-util 已依赖（Cargo.toml:32）。`serde_json`/`thiserror` 已依赖。chrono **绝不引入**（codex 用它打时间戳，zhive 不需要——时序若要记可用 std::time::Instant，但本设计不暴露时间字段，规避）。
- unsafe：零。子进程 spawn 全走 tokio 安全 API（bash.rs 已先例）。
- unwrap/expect：非测试码全 `?`/match。serde_json::to_string(event) → `?`（Serialize 变体）；stdin.take() → `if let Some`；write_all → match Err 降级；spawn → match Err 降级；child.wait_with_output() Result → map_err 降级；exit_code Option → match。`#[error]` 复用 thiserror（与 HookHostError/ValidatorError 同风格，不平行造轮）。
- 公开 API doc+doctest：SubprocessSpec、register_subprocess_hook、HookExecutor 均加 doc comment + 至少一个 doctest（SubprocessSpec 构造 + register_subprocess_hook 注册 echo 的 doctest 易写，doctest 内不真 spawn 以保确定性，或用 `no_run` 标注真 spawn 例）。run_subprocess_hook 是 pub(crate) 不需 doctest 但需 doc。
- 单文件 <600 行：subprocess 执行体拆到独立 `hooks/subprocess.rs`（~280 行），mod.rs 净增约 60 行（enum+字段+register_subprocess_hook+dispatch match 分支），仍 <600（当前 611 行含测试，需留意：把 subprocess 相关测试放 subprocess.rs，mod.rs 主体不超）。
- clippy -D warnings 风险点：`clone_on_ref_ptr`（workspace lint warn，root Cargo.toml:172）—— HookExecutor clone 时对 Arc 用 `Arc::clone(&x)` 而非 `.clone()`；`empty_enum_variants_with_brackets` —— SubprocessHookError::Cancelled 用 unit variant（无括号）。

## crossModuleDeps
- A4(proto hook schema) ↔ 本特性：stdin = serde_json::to_string(HookEvent)，依赖 hook.rs:175 内部 tag wire 稳定。基线 (a) Unknown variant 落地后，子进程**不会**收到 Unknown（Unknown 不可订阅、不主动构造，A4:797），故 subprocess serialize 端不触及 Unknown，无冲突。HookEvent Serialize 必须对所有 14+1 variant 正确（已验证 round-trip 绿）。
- permission.rs(HookOutput wire) ↔ 本特性：stdout = serde_json::from_str::<HookOutput>。子进程返回的 HookOutput 与 in-process HookFn 返回的同型，dispatch 折叠（tool_dispatch/mod.rs:433-485）对二者无差别处理 —— 这是「subprocess 透明接入现有 dispatch」的关键耦合点，要求 HookOutput 的 Deserialize 对 CC wire 完整（permission.rs:557 已 camelCase + #[serde(default)] 兜全 Optional）。
- validator.rs(SchemaCache 红线11) ↔ 本特性：subprocess hook 若返回 updated_input（HookSpecificOutput::PreToolUse.updated_input），重验证仍走 tool_dispatch/mod.rs:464 hook_host.schemas().revalidate()，**subprocess 模块不自己校验**（职责单一：只跑协议、返 HookOutput；红线11 在 dispatch 下游统一执行）。这要求 subprocess 不绕过折叠路径直接改 tool_input。
- 基线 hooks-enhance.md (b)timeout ↔ 本特性：HookRegistration.timeout 字段二者共用。Subprocess 的 effective_timeout = timeout.unwrap_or(DEFAULT_SUBPROCESS_TIMEOUT=600s)；InProcess 的 timeout 走基线 b 的 tokio::time::timeout 包裹。两条路径读同一字段，需在 register_subprocess_hook 与 register 里都正确填 timeout。
- 基线 hooks-enhance.md (d)signal ↔ 本特性：dispatch_with_signal 的 CancellationToken 必须透传到 run_subprocess_hook 的 select! cancel 分支。本阶段两调用点(tool_dispatch:413/compaction:223)仍传 never-cancelled 哨兵或保旧 dispatch()，真实 turn-cancel 注入随 B7 落地——故 subprocess 的 cancel 在本阶段功能可测（构造 pre-cancelled token 验证子进程被 kill），但生产接线待 B7。
- A5 manifest entrypoint ↔ 本特性：Phase 1 entrypoint 仅 builtin（decision-diffs §35/267），manifest loader **不会**构造 SubprocessSpec。故 register_subprocess_hook 当前仅由测试/未来 Phase 2 loader 调用。本特性提供完整执行体能力但不接 manifest——这是用户「全量执行体、即使 loader 暂不构造」要求的精确落点：能力齐全、入口暴露、测试覆盖 round-trip，manifest 接线留 Phase 2（A5:71 候选(b) cmd: 形态）。
- engine 构造点(inner.rs:158/engine.rs:221/subagent_spawn.rs:888) ↔ 本特性：register 旧签名保留 → 这些 HookHost::new() + 无 subprocess 注册的路径零改动。

## tests
- subprocess round-trip(核心)：register_subprocess_hook 注册一个 `sh -c 'cat'` 子进程 hook（把 stdin 原样吐 stdout 不行——需吐 HookOutput JSON）；改用 `sh -c 'cat >/dev/null; printf {...HookOutput JSON...}'` 验证 stdin 收到了正确序列化的 HookEvent、stdout 的 HookOutput 被正确解析并出现在 dispatch 返回的 Vec<HookOutput>。
- subprocess exit0 empty stdout → 无意见：`sh -c 'cat >/dev/null; exit 0'` → dispatch 返回空 Vec（None 不 push）。
- subprocess exit2 with stderr → block：注册 PreToolUse 子进程 `sh -c 'cat >/dev/null; echo blocked-reason >&2; exit 2'` → 合成 HookOutput 含 PreToolUse{permission_decision:Deny, permission_decision_reason:Some("blocked-reason")}。
- subprocess exit2 without stderr → 非阻塞：`sh -c 'exit 2'` → warn + None（不 block）。
- subprocess exit!=0,!=2 → 非阻塞错误：`sh -c 'exit 1'` → warn + None。
- subprocess timeout → kill + 无意见 + dispatch 继续：注册 `sh -c 'sleep 30'` timeout=Some(50ms) + 一个 InProcess counting hook → subprocess 超时被 kill、counting hook 仍跑、dispatch Ok（进程隔离验证）。
- subprocess spawn 失败 → 跳过不连累：program="/nonexistent/binary" → warn + None + dispatch Ok。
- subprocess invalid stdout JSON → 跳过：`sh -c 'echo not-json; exit 0'` → looks_like_json=false → None（当纯文本忽略）；`sh -c 'echo {bad; exit 0'` → looks_like_json=true 但解析失败 → warn + None。
- subprocess cancel 短路：pre-cancelled CancellationToken → run_subprocess_hook 返回 Err(Cancelled)、dispatch break、子进程被 kill_on_drop 清理。
- subprocess cwd/env 生效：注册 `sh -c 'echo \"{...}\" with env $FOO and pwd'`，spec.env=[(FOO,bar)] + spec.cwd=tempdir → 验证子进程看到 FOO=bar 与 cwd（用一个会把 env/pwd 写进 HookOutput.system_message 的脚本回读）。
- PostToolUse exit2 不可 block：注册 PostToolUse 子进程 exit 2 + stderr → event_can_block(PostToolUse)=false → 不合成 block HookOutput（对齐 CC）。
- register_subprocess_hook 红线10：registered_by id/version 空 → MissingProvenanceId/Version（复用现有校验）。
- InProcess 路径回归：现有 callback_panic_is_isolated(mod.rs:574)、registrations_sorted_by_priority(mod.rs:482)、dropping_scope_deregisters(mod.rs:549)、dispatch_runs_serially(mod.rs:529) 全绿（executor 重构不破坏）。
- doctest：SubprocessSpec 构造、register_subprocess_hook（no_run 真 spawn 或纯构造）。

## risks
第一坑（最大）：stdin 不 shutdown 导致子进程读 stdin 卡死到 timeout。必须在 write_all 后显式 `stdin.shutdown().await` 或 drop stdin 句柄给 EOF（bash.rs 用 Stdio::null() 规避，但 subprocess 要喂数据故必须 take+write+shutdown）。codex command_runner.rs:56-58 take 后写完未显式 shutdown 但 `wait_with_output` 内部会 drop stdin —— zhive 用 select! 时 stdin 在 take 后已脱离 child，需手动 drop/shutdown，否则子进程可能不收 EOF。建议 write_all 后 `drop(stdin)` 或 `stdin.shutdown().await` 再 select wait。

第二坑：mod.rs 行数。当前 611 行（含测试）。重构加 enum+字段+register_subprocess_hook+match 分支约 +60 行；务必把 subprocess 的全部测试放 subprocess.rs，否则 mod.rs 破 600 行触发「考虑拆分」（软约束但要遵守）。

第三坑：HookExecutor clone 触发 clippy clone_on_ref_ptr（workspace warn=error）。dispatch 快照 clone executor 时，InProcess/Subprocess 内都是 Arc，必须 `Arc::clone` 显式，不能派生 #[derive(Clone)] 后 .clone()（派生会用 .clone() 触发 lint）——手写 Clone 或快照时 match 取 Arc::clone。

第四坑：exit==2 合成 HookOutput 的形态对不齐下游折叠。tool_dispatch/mod.rs:438-453 折叠只认 HookSpecificOutput::PreToolUse{permission_decision, updated_input} 与 continue_loop。exit2 block 必须造 permission_decision=Deny（不是 continue_loop=false，那是「全局 abort」语义不同）。对 UserPromptSubmit/Stop 等无 PreToolUse hookSpecificOutput 的 event，exit2 block 当前 dispatch 下游（compaction.rs 等）如何消费需确认——本阶段 subprocess 主要服务 PreToolUse 路径（红线11/权限），其余 event 的 block 语义下游接线随各自 consumer 落地，subprocess 模块只忠实合成 HookOutput，不替下游决定。

第五坑：测试可移植性。用 `sh -c` 在 Windows 不可用；zhive 目标平台是 Linux（env Linux cachyos），但测试应 `#[cfg(unix)]` 标注 sh 依赖测试，避免 CI 跨平台炸（参照 bash.rs 测试惯例）。

第六坑：kill_on_drop 仅 kill 直接子进程，不 kill 孙进程（子进程 fork 的）。与 bash.rs 同等局限，Phase 1 可接受（CC 也是进程组语义留待加固）；不在本阶段引入 process group/setsid（那需 unsafe 或 rustix，超范围）。

低风险：进程隔离本身比 in-process catch_unwind 更强——子进程 segfault/OOM 不可能污染 engine。serde wire 复用零新类型。

## recommendation
实现顺序（与基线 hooks-enhance.md 合并）：

1. 先落基线 (a)Unknown(proto 独立) → (b)timeout(core) → (d)signal(core) 的结构骨架（HookExecutor enum + HookRegistration.executor/timeout 字段 + register 旧签名包 InProcess + dispatch_with_signal）。这一步把双轨结构与 timeout/signal 入口建好，是本特性的承载地基。

2. 本特性主体：新建 hooks/subprocess.rs，实装 run_subprocess_hook 完整协议（spawn→stdin JSON→shutdown→select{wait|timeout|cancel}→exit-code 解析→HookOutput）。**照抄两个锚点**：子进程机制抄 zhive 自己的 bash.rs:159-209（kill_on_drop+pin+select!，风格一致且接 cancel），exit-code/JSON 语义抄 codex pre_tool_use.rs:200-286 + output_parser.rs:120-181/337-355。

3. mod.rs dispatch_inner 接线：match executor，Subprocess 调 run_subprocess_hook；新增 register_subprocess_hook 公开 API。

4. 测试：subprocess.rs 内 #[cfg(unix)] 用 sh -c 做 round-trip/exit0/exit2/timeout/spawn-fail/cancel/cwd-env 全覆盖（schema tests 字段要求）。

== 与基线设计的合并取舍 ==
- **保留基线**：双轨 enum 思路、register 旧签名零破坏、HookExecutor 命名、(a)(b)(d) 三件全做、串行 dispatch（不抄 codex 并发，因红线11 mutate 链）。
- **扩展基线**：把基线 (c) 的「Subprocess skip+warn 占位 + Phase 2 才落协议」**升级为本设计的真实执行体**（用户拍板档B全量）。基线 recommendation 第3点「Subprocess 仅占位、协议推 Phase 2」**本次作废**——改为完整实装。
- **不扩展**：manifest loader 仍不构造 SubprocessSpec（A5 entrypoint Phase 1 仅 builtin，decision-diffs §267 不变）——执行体能力齐全但入口仅测试/Phase2 用，符合「即使 loader 暂不构造也要能注册并执行」的精确要求。

每步独立验证：`cargo check -p zhive-core --lib` → `cargo nextest run -p zhive-core hooks` → 最后 `cargo fmt --check && cargo clippy -p zhive-core -- -D warnings`。写 .rs 前先读 ms-rust skill（命中新增 public API/trait/error 类型规则）。
