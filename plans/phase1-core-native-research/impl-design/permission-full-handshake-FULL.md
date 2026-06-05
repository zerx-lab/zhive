# 【全量】父子 permission reducer 完整逐 tool-call 实时握手（档 B 生产版）

## foundation
基线底座（全量版在其上叠加）：
- Defer wire/Suspended 三件由 permission-suspend-resume.md 覆盖：PermissionOutcome::Defer、ResumeOutcome、ResumePermissionParams、TurnSuspended/Resumed、SubagentFinalEvent::Suspended — 全量版不改其形状，只新增子→父逐 tool-call 通道。

全量握手所复用的既有架构（设计约束）：
- reducer 基础设施直接复用：crates/zhive-core/src/permission.rs PermissionReducer::{enroll,wait,wait_unbounded,resolve_by_wire_id,cancel_all}，evaluate（含 BypassPermissions 短路），PendingPermissions（permission/pending.rs，单 EngineInner 全局共享一个 reducer，Arc）。
- 子 turn 在独立 tokio task：subagent_spawn/mod.rs；父 turn 在另一独立 task（inner.rs start_turn → turn.rs run_turn）。两 task 之间的 final channel 是死锁与握手设计的核心约束点。
- SubagentSpawner trait：crates/zhive-core/src/tools.rs:181-200，spawn_and_await(name,description,prompt)->Result<String,String>，被 builtin/agent.rs 调用。父对子的「等待 final」发生在父 turn 的 tool execute 阶段（execute_resolved_tool → agent 工具 → spawn_and_await）。

## harnessRef
**唯一权威全量参考 = codex（不是 pi）**。

pi 无 subagent、无引擎内 permission 拦截：grep packages/agent/src 与 coding-agent/src 对 subagent/spawnAgent/permission-in-loop 全空（agent-loop.ts 无 permission/approval/task 任何引用）。pi 的 approval 在 client 侧 interactive-mode.ts，不是父子引擎握手。**故 pi 不可照抄；本特性的全量模型对齐 codex。**

**codex 的逐 tool-call 实时父子握手（精确文件:行号）— 这就是全量蓝本**：
- ~/Desktop/code/github/codex/codex-rs/core/src/codex_delegate.rs：
  - run_codex_thread_interactive（:65-160）/ run_codex_thread_one_shot（:164-243）：子 agent 是独立 Codex（独立 submission/event channel，:75-76 tx_sub/rx_sub + tx_ops/rx_ops），但**共享父的 services**（exec_policy/skills/mcp/thread_store，:82-103）且 forked_from_thread_id=parent（:89），cancel 用 cancel_token.child_token()（:121-122,:178）使父 cancel 级联子。
  - **关键：forward_events 转发任务**（:245-384）。子 agent 跑自己的 loop，所有事件流到这个 task；它 **filter 出 approval 类事件，路由到父 session 决策，绝不透传给最终消费者**：ExecApprovalRequest→handle_exec_approval（:276-290）、ApplyPatchApprovalRequest（:291-304）、RequestPermissions→handle_request_permissions（:305-317）、RequestUserInput（:318-332）；其余事件 forward_event_or_shutdown 透传（:374-379）。
  - **决策回传子的机制**（握手的「返回边」）：handle_exec_approval（:435-515）调父 parent_session.request_command_approval(...)（:487-499）拿 decision，然后 codex.submit(Op::ExecApproval{id,turn_id,decision})（:508-514）发回子；子的 tool dispatch 此前 park 在等这个 approval id 的 pending 上。handle_request_permissions 同形：parent_session.request_permissions_for_cwd（:756-762）→ codex.submit(Op::RequestPermissionsResponse{id,response})（:765-769）。
  - **父侧二次决策的 reverse-RPC + cancel/超时传播**：await_approval_with_cancel（:829+）/ await_request_permissions_with_cancel（:802+）/ await_user_input_with_cancel（:772-799）全部 tokio::select!{ biased; _=cancel_token.cancelled()=>空响应兜底; response=fut=>... }（:786-798）。cancel 时给子回「拒绝/空」决策让子不挂死，并 notify 父 session。
  - shutdown_delegate（:387-402）：父决定终止子时 submit Interrupt+Shutdown 并 drain 子事件直到 TurnComplete/Aborted。

**映射到 zhive**（codex 用「独立 Codex + 双 async_channel + forward task」，zhive 用「同 EngineInner + 新 ThreadHandle + 共享 reducer」更轻）：
- codex forward_events filter approval → 父决策 → codex.submit(Op::*) 回子 ≈ zhive 新增「子 dispatch 在 fold 出非-Allow 决策时经 subagent_decision_tx 发 SubagentDecisionRequest{tool_use_id,tool_name,raw_args,child_decision,reply:oneshot<ParentVerdict>} 给父 spawner，父 fold（可再 enroll/reverse-RPC 或 Defer），经 reply oneshot 回子」。
- codex 子 tool park 等 Op::ExecApproval ≈ zhive 子 resolve_tool_permission_inner 在 evaluate 之后、execute 之前 park 在 reply oneshot 上。
- codex await_*_with_cancel 的 biased cancel select ≈ zhive 子等父 reply 时 tokio::select!{ biased; ()=cancel.cancelled()=>Deny兜底; d=reply_rx=>d }，复用 child_cancel（subagent_spawn.rs:192）。
- codex 父 request_command_approval 二次 = zhive 父侧把 child_decision 当「额外一项 hook」append 进父 decisions 再 evaluate(parent_scope,&decisions)（A3 §7.4 字面），父可再触发 enroll→PermissionRequested→wait（已有 reducer 全套）。

## approach
## 全量目标与不变式（A3 §7.1 三不变式必须 teeth）
逐 tool-call：子每次 fold 出 child_decision 后**执行前实时上报父**，父对 [parent_hooks..., child_decision] 二次 evaluate（父更严不可放大：Deny>Defer>Ask>Allow 单调，父 fold 只能等于或严于 child；codex 同语义），父决策（可能含父侧 reverse-RPC 问用户、或父 Defer）经 oneshot 回子，子据此 Allow→execute / Deny→block / Ask 实为父已解出的 Allow|Deny / Defer→子 park 待父 resume。

## 握手协议（请求-响应往返，与基线 Defer/Suspended 协同）

### 新 channel 拓扑（三条 in-process channel，全程不走 wire）
1. subagent_final_tx（已存在，容量 1）：子→父，Completed/Errored/**Suspended**（基线已加 Suspended）。
2. **新增 subagent_decision_tx: mpsc::Sender<SubagentDecisionRequest>（容量 1，子 PHASE1 串行）**：子→父，载 {tool_use_id, tool_name, raw_args, child_decision, reply: oneshot::Sender<ParentVerdict>}。
3. reply oneshot（每次握手新建，藏在 #2 payload 里）：父→子，回 ParentVerdict。

父侧谁收 #2？**父 turn task 不能收**（它此刻阻塞在 agent 工具 execute 内的 spawn_and_await），所以由 **EngineSubagentSpawner::spawn_and_await 内的 select 循环**统一收（全量版改持有 final rx + decision rx 两者）。父在该函数里 select 两条 rx：收到 SubagentDecisionRequest→父侧二次 fold（必要时 reverse-RPC/Defer）→reply 回子→继续 loop；收到 Completed/Errored→返回 final（退 loop）；收到 Suspended→父转发 TurnSuspended 且继续 loop 等 resume。

### 完整时序图（文字）
父 turn task（turn.rs run_turn）execute agent tool → spawn_and_await（select loop on final_rx|decision_rx）。
子 turn task（run_child_turn_inner→run_turn→tool_dispatch）串行 resolve 每个 tool-call：scope.permits 门控(已有:354) → PreToolUse hooks → child_decision=evaluate(child_scope,child_hooks)。
若 child_decision==Deny：不上报，直接 block（短路，省往返）。
否则：建 reply oneshot，send SubagentDecisionRequest 给父，park select(cancel|reply)。
父收 request → 二次 fold：decisions=[parent_hooks...,child_decision]；parent_verdict=evaluate(parent_scope,decisions)。若 ==Ask：enroll→PermissionRequested(events_tx)→wait(bounded)→Allow|Deny。若 ==Defer：enroll→PermissionRequested→父发 TurnSuspended(父turn_id+req_id)→wait_unbounded→resolve 后发 TurnResumed。最终 verdict∈{Allow,Deny}。
父 req.reply.send(ParentVerdict(verdict)) → 子 reply_rx 解出 → Allow→execute / Deny→block → 子继续下一个 tool-call。
子 turn 末帧 final → deliver_subagent_outcome 发 Completed{final} → 父 spawn_and_await 返回 → 父 turn 续。

### 死锁规避（核心难点）
- **父不在 turn task 等子，而在 spawn_and_await（仍是父 turn task 的子帧）等**：父 turn task 调 execute_resolved_tool→agent tool→spawn_and_await，整段在父 turn task 内同步 await。子的 decision 请求由这个 await 内的 select 循环消费——不存在「父 turn task 在别处忙、无人收 decision」的窗口。
- **父二次 fold 触发父 reverse-RPC（Ask/Defer）时谁驱动父 reducer.wait？** 就是 spawn_and_await 这个 future（父 turn task 帧内）。父 PermissionRequested 经 events_tx 广播给外部 client；client 经 engine/resume_permission（actor dispatch，inner.rs:488 resume_permission）resolve。actor dispatch 在**另一个** task（EngineInner submission loop），不是父 turn task —— 故父 turn task 阻塞在 wait 不卡 resume 投递。✅ 无死锁。
- **子 park 等 reply 时父 cancel**：子 select biased cancel 臂（复用 child_cancel）先于 reply 触发 → 子按 Deny 兜底 block 该 tool → 子 turn 收尾 → 发 Completed/Errored → spawner loop 退出。父 cancel 级联子（child_for_turn 派生，subagent_spawn.rs:192）。
- **reply oneshot drop（父 panic）**：子 reply_rx.await 得 RecvError → Deny 兜底（绝不放大），子继续。
- **decision_rx 父侧已退 loop 但子还想发**：mpsc send Err → 子 Deny 兜底 block。

### 与基线 Defer/Suspended 协同
- 子自身 fold 出 Defer（child_decision==Defer）：全量版下子的 Defer 仍**先上报父**（不短路，与 Deny 短路相反）。父 evaluate([parent_hooks,Defer]) 得 Defer→父也挂起（两层 Suspended，基线已设计），父 reply 回子……此处需细化：子 Defer 时子既要 wait_unbounded（基线发 Suspended）又被父挂起，收敛为：子 fold Defer→先上报父，父若得 Defer 则父 reply 一个特殊「子自挂起」指令？为避免子父双 wait_unbounded 竞争，采用：子 fold==Defer 时子**不自己 wait_unbounded**，一律上报父，由父统一 enroll+wait_unbounded+发 TurnSuspended，父 resume 后把终态 Allow/Deny 经 reply 回子（子永不进 wait_unbounded 分支，子的 Ask/Defer 自处理分支仅在 decision_tx.is_none() 顶层 turn 走）。这统一了「子永不自己 reverse-RPC」原则。
- 父二次 fold 自己得 Defer（父 hook Defer，子 Allow）：父挂起（发 TurnSuspended），子 park 在 reply 等父 resume 后终态——「父对子单步实时挂起干预」最强体现，子不发 Suspended（子没 defer）。

### 短路规则（A3 §7.4 + codex 优化）
- child_decision==Deny → 不上报直接 block（Deny 父不可能放松，省往返；codex 同理）。
- child_decision∈{Allow,Ask,Defer} → 上报父二次 fold（Ask/Defer 在子侧不自己 enroll，交父统一——避免父子双弹，对齐 codex「approval 一律路由父」）。

## 为什么这样做（而非基线轻量双调）
基线轻量版只「子非 Allow 上报父二次否决、不阻塞子」，不满足档 B「执行前实时、父对每步有干预能力（含 reverse-RPC/Defer）」。全量版让子**真正 park 等父 verdict 再 execute**，这才是 codex 语义，也才让 A3「reducer 父子各执行一次」在每个 tool-call 上成立。代价是子 resolve 多一次 await 往返，但子 dispatch PHASE1 本就串行（turn.rs:482-547），不破坏并发模型。

## files

- `crates/zhive-core/src/subagent.rs` — (1) SubagentFinalEvent 加 Suspended{child_thread_id,child_request_id:String}（基线）。(2) 新增 pub(crate) struct SubagentDecisionRequest{tool_use_id:String, tool_name:String, raw_args:serde_json::Value, child_decision:PermissionDecision, reply: tokio::sync::oneshot::Sender<ParentVerdict>} + pub enum ParentVerdict{Allow,Deny}（父侧最终 verdict，类型级禁 Ask/Defer 回子）。(3) 删 :39-41「Suspended 省略」段，补逐 tool-call 握手落地说明 + doctest。SubagentDecisionRequest 含 oneshot 不能 derive Clone/Debug，手写 Debug 跳过 reply 字段（finish_non_exhaustive）。
- `crates/zhive-core/src/state/thread.rs` — ThreadHandle 加 subagent_decision_tx: Option<tokio::sync::mpsc::Sender<crate::subagent::SubagentDecisionRequest>>（:75 旁）。new_child 改为返回三元组 (Self, Receiver<SubagentFinalEvent>, Receiver<SubagentDecisionRequest>)，所有 new_child test 调用点机械改 (h,_rx,_drx)。new_idle/with_capacity 置 None。decision channel 容量 1。更新 doctest。
- `crates/zhive-core/src/engine/tool_dispatch/mod.rs` — resolve_tool_permission(+_inner) 签名加 decision_tx: Option<&tokio::sync::mpsc::Sender<SubagentDecisionRequest>> 参数。evaluate(scope,&decisions)（:488）得 child_decision 后：若 decision_tx.is_some()（子 turn）：child_decision==Deny→直接 block(短路)；其余→建 reply oneshot、send SubagentDecisionRequest（携 tool_name+raw_args）、tokio::select!{biased; ()=cancel.cancelled()=>block(Deny兜底); v=reply_rx=>match{Ok(Allow)=>Approved, Ok(Deny)|Err=>block}}。**子不走 :503-757 的自身 Ask/Defer reverse-RPC——该整段用 decision_tx.is_none() 守卫（顶层 turn 零回归）**。子侧握手抽 helpers.rs 内 async fn handshake_with_parent(...) 控行数。
- `crates/zhive-core/src/engine/turn.rs` — run_turn_inner 读 scope（:169-174）同时读 handle.subagent_decision_tx；PHASE1 调 resolve_tool_permission（:505-518）多传 decision_tx（顶层 None、子 turn Some）。其余不变。
- `crates/zhive-core/src/engine/subagent_spawn.rs (拆出 spawner.rs)` — (1) spawn_subagent_awaitable（:96-224）new_child 拿三元组，返回类型改 (ThreadId, Receiver<SubagentFinalEvent>, Receiver<SubagentDecisionRequest>)；spawn_subagent（actor path,:70-81）丢弃两 rx。(2) EngineSubagentSpawner::spawn_and_await（:517-548）重写为 select 循环：loop{ select!{ Some(req)=decision_rx.recv()=>{verdict=self.parent_second_fold(&req).await; let _=req.reply.send(verdict)}; final=final_rx.recv()=>match Completed=>return Ok(text)/Errored=>return Err/Suspended=>{父转发 TurnSuspended; continue}; None=>return Err("channel closed") } }。新增私有 async fn parent_second_fold(&self,req)->ParentVerdict：取 parent_handle.active_turn.scope; 构 PreToolUse HookEvent（同 tool_dispatch:381 模板, 用 req.tool_name/raw_args）; dispatch parent hooks; decisions=[hooks...,req.child_decision]; v=evaluate(parent_scope,decisions); 若 Ask→enroll/events_tx PermissionRequested/wait(bounded)→Allow|Deny; 若 Defer→enroll/PermissionRequested/events_tx TurnSuspended/wait_unbounded/resolve后 TurnResumed→Allow|Deny; 返回 ParentVerdict。拆到新文件控 600 行。
- `crates/zhive-core/src/engine/event.rs` — EngineEvent 加 TurnSuspended{thread_id,turn_id,request_id:PermissionRequestId,reason:Option<String>} + TurnResumed{thread_id,turn_id}（:159 旁，基线项；父二次 fold 得 Defer 时父发 TurnSuspended）。
- `crates/zhive-proto/src/permission.rs` — 基线项（全量沿用不改）：PermissionOutcome::Defer{reason}、ResumeOutcome{Selected/Cancelled}、ResumePermissionParams、TurnSuspendedNotification、TurnResumedNotification、method 常量 + doctest。
- `crates/zhive-core/src/server/events.rs` — 基线项：engine_event_to_notification 加 TurnSuspended→events/turn_suspended、TurnResumed→events/turn_resumed 映射 + method 名单测。
- `crates/zhive-core/src/server/handlers.rs` — 基线项：删本地 ResumePermissionParams 改用 proto；ResumeOutcome→PermissionOutcome via From；session/resume_permission 别名双注册保留 engine/resume_permission。
- `crates/zhive-core/src/engine/inner.rs` — resume_permission（:488-520）共享 PendingPermissions，resolve_by_wire_id 直接命中父 parent_second_fold enroll 的 req_id——两层 resume 免子专属 map。可选：resolve 成功后发 EngineEvent::TurnResumed。无结构性新增。

## newTypes
- pub(crate) struct SubagentDecisionRequest { pub tool_use_id: String, pub tool_name: String, pub raw_args: serde_json::Value, pub child_decision: PermissionDecision, pub reply: tokio::sync::oneshot::Sender<ParentVerdict> }  (core::subagent；含 tool_name/raw_args 供父 PreToolUse hook 重新 dispatch——父对子的二次审查必须能看到 tool 上下文；手写 Debug 跳过 reply；不 Clone)
- pub enum ParentVerdict { Allow, Deny }  (core::subagent；#[non_exhaustive]；父侧最终 verdict——类型级保证回子的只能是终态，Ask/Defer 已在父 spawner loop 内解完)
- SubagentFinalEvent::Suspended { child_thread_id: ThreadId, child_request_id: String }  (基线项；pub variant 加 doctest)
- ThreadHandle.subagent_decision_tx: Option<tokio::sync::mpsc::Sender<crate::subagent::SubagentDecisionRequest>>
- ThreadHandle::new_child(id, parent_id) -> (Self, Receiver<SubagentFinalEvent>, Receiver<SubagentDecisionRequest>)  (改三元组；所有 test 机械适配)
- fn resolve_tool_permission(..., decision_tx: Option<&tokio::sync::mpsc::Sender<SubagentDecisionRequest>>) -> ToolResolution  (+ _inner 同步加参；顶层 turn 传 None；子 turn 传 Some)
- EngineSubagentSpawner::parent_second_fold(&self, req: &SubagentDecisionRequest) -> ParentVerdict  (新私有 async；dispatch parent PreToolUse hooks + evaluate + 必要时 reverse-RPC/Defer + 发 TurnSuspended/TurnResumed；复用 reducer.enroll/wait/wait_unbounded + events_tx)
- EngineInner::spawn_subagent_awaitable -> Result<(ThreadId, Receiver<SubagentFinalEvent>, Receiver<SubagentDecisionRequest>), SubagentSpawnError>  (返回类型改三元组)
- EngineEvent::TurnSuspended { thread_id, turn_id, request_id: PermissionRequestId, reason: Option<String> } + EngineEvent::TurnResumed { thread_id, turn_id }  (基线项；#[non_exhaustive] enum 加 variant 非 breaking)
- proto (基线项): PermissionOutcome::Defer{reason:Option<String>}、pub enum ResumeOutcome{Selected{option_id},Cancelled}、pub struct ResumePermissionParams、pub struct TurnSuspendedNotification、pub struct TurnResumedNotification、impl From<ResumeOutcome> for PermissionOutcome

## redlineImpact
无触红线：
- **不新增 crate**：全部复用 tokio::sync::{mpsc,oneshot}、tokio_util CancellationToken、thiserror、serde/serde_json、schemars(feature=schema)、tracing、futures。codex 用 async_channel，zhive 用 tokio mpsc/oneshot 等价替代——无需引入 async-channel。
- **无 unsafe**。
- **无非测试 unwrap()/expect()**：子等 reply 用 match reply_rx.await { Ok(Allow)=>.. , Ok(Deny)|Err(_)=>block }；父 req.reply.send(verdict) 忽略 Err 用 let _=（子已走人）；mpsc recv 返回 Option，None→收尾；parent_scope 取用现有 .map_or_else(default_turn_scope,...) 模式（subagent_spawn.rs:117-122 已有）。
- **公开 API doc+doctest**：SubagentDecisionRequest 是 pub(crate) 仅 doc comment；ParentVerdict 若 pub 需 doctest（建议 pub(crate) 省 doctest，但若 SubagentFinalEvent 已 pub 牵连可 pub+doctest）。SubagentFinalEvent::Suspended pub variant→补 doctest（subagent.rs 现有模式 :45-56）。proto 新类型全 pub→serde round-trip doctest（permission.rs 现有模式）。EngineEvent variant 沿例文档注释。
- **复用现有 error**：不新增 ReducerError variant；父二次 fold reverse-RPC 失败复用 TimedOut/Abandoned；reply 通道失败不入 error 类型，直接 Deny 兜底（语义安全降级，与 tool_dispatch:692-731 的 Cancelled/Err→Deny 一致）。
- **#[non_exhaustive]** 沿用：ParentVerdict、新 EngineEvent variant、proto 新 enum 全加。
- **600 行软限**：subagent_spawn.rs 现 953 行（已超，含大量 test）；spawner 逻辑（spawn_and_await 重写 + parent_second_fold）必须拆到新文件 engine/subagent_spawn/spawner.rs（impl EngineInner 跨文件已是项目模式，见 subagent_spawn.rs:7-10）。tool_dispatch/mod.rs 现 1083 行（已超）；子侧握手抽进 helpers.rs handshake_with_parent。
- **机械适配非红线**：new_child 改三元组波及 ~6 处 test let (h,_rx)=new_child → let (h,_rx,_drx)，纯机械。

## crossModuleDeps
- 与 fork↔state-lazy-load（重点耦合）：本特性给 ThreadHandle 加第三条 channel（subagent_decision_tx）。decision channel 生命周期严格内含于一次子 turn（active_turn 存在期间子 thread 常驻内存，绝不被换出/复活），不跨持久化边界。给 fork/lazy-load owner 的精确接口需求：(a) lazy-load 复活【已结束】子 thread 一律走 new_idle（decision_tx=None），ThreadHandle::new_child(三元组) 是唯一建 decision channel 的入口；(b) fork 含【活跃】子 turn 的父是未定义场景——需 fork owner 确认 fork 仅作用于 idle thread，否则会复制悬空 decision_rx，必须禁止或快照时丢弃 in-flight 子握手。
- 与 server transport/handlers：父二次 fold 的 reverse-RPC 经现有 events_tx.send(EngineEvent::PermissionRequested)（就是父 turn 的 PermissionRequested），client 经 engine/resume_permission resolve——无新 wire surface（复用基线 Defer/resume 通道）。需知会 transport owner：subagent 场景 client 收到的 PermissionRequested thread_id 是父 thread（父代子问），request_id 全局唯一，client 无需区分父原生还是代子。
- 与 ACP bridge：父二次 fold 产生的 PermissionRequested 走标准 outcome_to_engine（bridge-acp/src/permission.rs），ACP 无 Defer 的 _=>Cancelled 兜底仍编译通过。SubagentDecisionRequest/ParentVerdict 纯 in-process，bridge 不可见。
- 与基线 permission-suspend-resume topic：本特性【依赖】基线先落地 proto 层（Defer/ResumeOutcome/TurnSuspended/Resumed）+ SubagentFinalEvent::Suspended + EngineEvent::TurnSuspended/Resumed。实现顺序基线 ①②③ 必须在全量握手 ④ 之前。两者都给 SubagentFinalEvent 加 variant——建议基线加 Suspended，本特性加独立的 SubagentDecisionRequest channel，互不冲突。
- 与 B7 cancel-streaming：父/子 cancel 经 child_for_turn 派生（subagent_spawn.rs:192）级联；cancel_all 把父二次 fold 挂起的 pending 解为 Cancelled（permission.rs:326）；子 park 在 reply_rx 时 cancel 走 biased cancel 臂→Deny。无新增清理。

## tests
- core: 子 fold Allow → 上报父 → 父 hook 也 Allow → ParentVerdict::Allow → 子 execute（断言 tool 真跑）。
- core: 子 fold Allow → 父 hook Deny → ParentVerdict::Deny → 子 block（断言 ToolCall status=Failed、tool 未跑）。验证父更严 teeth。
- core: 子 fold Deny → 不上报（断言 decision_rx 未收到）→ 子直接 block（短路）。
- core: 子 fold Ask → 上报父 → 父 enroll PermissionRequested（断言 events_tx 收到，thread_id=父）→ resolve(Selected allow_once) → ParentVerdict::Allow → 子 execute。验证问用户由父代理、子不自己弹。
- core: 父二次 fold 得 Defer（父 hook Defer）→ 父发 TurnSuspended（断言 events_tx，携父 turn_id+req_id）→ 子 park 在 reply_rx → resume(Selected) → 父 TurnResumed → ParentVerdict::Allow → 子续。验证父对子单步实时挂起干预。
- core 死锁回归：子等 reply 时 cancel 父 turn → 子 reply_rx 走 cancel 臂 → 子 Deny block → 子 turn 收尾发 Completed → spawner loop 退出（tokio::time::timeout 包测试体断言不挂死）。
- core: reply oneshot drop（模拟父 spawner 提前退出/panic）→ 子 reply_rx Err → Deny 兜底 block。
- core: 多 tool-call 子 turn（PHASE1 串行多个）→ 每个独立握手往返一次（断言 decision_rx 收到 N 次、顺序与 emit 一致）。
- core: parent_second_fold 单调性——给 child_decision=Allow + 任意 parent_hooks，断言 verdict==evaluate(parent_scope,[parent_hooks,Allow]) 且 verdict∈{Allow,Deny}（Ask/Defer 已内部解为终态）。
- 端到端(ScriptedModel): 父 turn 调 agent 工具 → 子跑含一次 bash tool-call → 父 hook 注入 Deny → 断言子 bash 未执行、父收到子 final（含被拒上下文）、父 turn 正常完成。
- 回归: 现有 spawn_subagent_*（subagent_spawn.rs:598-949）全部保持绿（机械改 new_child 三元组后）；permission.rs wait_unbounded_receives_cancelled_on_cancel_all（:422）保持绿；顶层 turn Ask/Defer 路径（decision_tx==None）零回归。
- proto doctest(基线): Defer/ResumeOutcome/TurnSuspended round-trip。

## risks
1. **死锁是头号风险**：父 turn task 在 spawn_and_await 内 select 双 rx；若父二次 fold 触发父 reverse-RPC，父 turn task 阻塞在 reducer.wait，而 resume 投递走 actor submission loop（另一 task，inner.rs）——已论证不死锁。但**若未来把 resume 也路由到 turn task 内处理，会立刻死锁**。必须在 spawner 文档明示：父侧 reverse-RPC 的 resolve 必须由 actor loop（非 turn task）驱动。已有架构满足，但脆弱不变式，加测试守护（cancel 回归 + resume-during-handshake 测试）。
2. **子永不自己 Ask/Defer reverse-RPC 是行为变更**：全量版把「子问用户」全收敛到父代理（对齐 codex），与基线「子自己 Ask/Defer」语义不同。好处：避免父子双弹、req_id 归属清晰（都挂父 thread）；风险：若已有测试假设子直接 enroll 会失败。必须用 decision_tx.is_some() 守卫严格区分顶层 turn（原行为）vs 子 turn（新行为），保证顶层 Ask/Defer 零回归。
3. **SubagentDecisionRequest 含 oneshot 不可 Clone/Debug**：mpsc<不可 Clone payload> 合法（send 移动所有权），但它绝不进 EngineEvent（broadcast 要求 Clone）——只在 in-process channel 流转。手写 Debug 跳过 reply 字段。
4. **subagent_spawn.rs / tool_dispatch/mod.rs 已双双超 600 行**：必须拆文件（spawner 逻辑 + 子侧 handshake helper），否则违反风格规则。拆分机械但需小心 impl EngineInner 跨文件（项目已用此模式，subagent_spawn.rs:7-10 注释说明）。
5. **父二次 fold 需 tool 上下文给父 hook**：SubagentDecisionRequest 必须携带 tool_name+raw_args（不只 tool_use_id+decision），否则父 PreToolUse hook 拿不到 toolName/toolInput 无法决策——这是相对基线 SubagentDecision{tool_use_id,decision} 的关键修正。
6. **性能**：每子 tool-call 多一次 mpsc+oneshot 往返 + 父 hook dispatch。子 PHASE1 本串行，影响可控；高频 tool-call 放大延迟，可接受（codex 同款开销），不优化。
7. **父二次 fold 的 BypassPermissions**：父 scope==BypassPermissions 时 evaluate 短路 Allow（permission.rs:101-106），父放行一切，符合 A3 §7 信任声明语义；子 hook 仍 dispatch（子侧 dispatch 保证）。无需特殊处理。

## recommendation
## 实现顺序（与基线合并：基线①②③ 保留，全量在其上叠 ④）
1. **先落基线 proto + Suspended 可观测**（permission-suspend-resume.md 的 ①②③）：proto 五类型 + SubagentFinalEvent::Suspended + EngineEvent::TurnSuspended/Resumed + server/events 映射 + handlers ResumeOutcome。这是全量握手的依赖底座，零风险先解锁。
2. **加 channel 拓扑**：ThreadHandle.subagent_decision_tx + new_child 改三元组（机械改 test）+ SubagentDecisionRequest/ParentVerdict 新类型（含 tool_name/raw_args 修正）。
3. **子侧握手**（tool_dispatch）：resolve_tool_permission(+_inner) 加 decision_tx 参数；evaluate 后按 child_decision 分流（Deny 短路 / 其余上报父 park reply）；子 Ask/Defer 自身 reverse-RPC 分支用 decision_tx.is_none() 守卫（顶层零回归）。抽 helper 进 tool_dispatch/helpers.rs 控行数。
4. **父侧握手**（EngineSubagentSpawner，拆新文件 spawner.rs）：spawn_and_await 改 select 双 rx 循环 + parent_second_fold（父 hook dispatch + evaluate + 必要时 reverse-RPC/Defer + TurnSuspended/Resumed）。spawn_subagent_awaitable 返回三元组。
5. **turn.rs 接线**：run_turn 读 handle.subagent_decision_tx 透传给 resolve（顶层 None / 子 Some）。
6. 全量测试（见 tests）+ 回归。

## 与基线设计如何合并
- **基线保留**：Defer wire、ResumeOutcome、TurnSuspended/Resumed、SubagentFinalEvent::Suspended、两层 suspend-resume 路由（单 reducer 共享 PendingPermissions、req_id 全局唯一）——这些是全量版底座，一字不改。
- **基线扩展（轻量双调→全量握手）**：基线 §approach(b)「轻量双调：子非 Allow 上报父二次否决、不阻塞子」**升级为**全量「子 park 等父 verdict 再 execute，父可 reverse-RPC/Defer 实时干预」。基线 SubagentDecision{tool_use_id,decision} **扩展为** SubagentDecisionRequest{tool_use_id,tool_name,raw_args,child_decision,reply}（加 reply oneshot + tool 上下文）。
- **基线收敛点修正**：基线让「子自己 Ask/Defer」，全量版改为「子不自己问、全部上报父代理」（对齐 codex codex_delegate.rs forward_events 把所有 approval 路由父 session）。顶层 turn 行为不变（decision_tx==None 走原 Ask/Defer 路径）。
- **harness 对齐声明**：pi 无此能力，全量模型 100% 对齐 codex codex_delegate.rs 的「child loop + 父 forward/intercept + 决策回传子 park 点」三段式，zhive 用同 EngineInner+共享 reducer 替代 codex 独立 Codex+双 channel，更轻且天然共享 PendingPermissions（两层 resume 免子专属 map）。

核心难度评级：困难（跨 task 请求-响应握手 + 死锁规避 + 与现有自洽 run_turn 架构融合），但有 codex 精确蓝本，风险可控。最大守护项：父 reverse-RPC 的 resolve 必须由 actor loop 而非 turn task 驱动（否则死锁），用测试钉死。
