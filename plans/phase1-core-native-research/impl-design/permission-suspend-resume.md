# Permission Defer wire + 父子 reducer 双调 + Subagent Suspended

## currentState
三件都半成品，比 gap 描述更前进，需精确对账：

(1) Defer 内核已通但 wire 不可观测。`PermissionDecision::Defer` 已存在 (zhive-proto/src/permission.rs:79)。Defer 路径在 tool_dispatch/mod.rs:503-618 走 `wait_unbounded`(503-511 注释 + permission.rs:277-282)，挂起期间**不发任何 notification**——客户端只看到先前的 `events/permission_requested`(server/events.rs:347)，无法区分「正在等用户」与「挂死」。`PermissionOutcome` 仅 Selected/Cancelled (permission.rs:465-473)，**无 Defer variant**。无 proto 级 `TurnSuspendedNotification`/`TurnResumedNotification`/`ResumeOutcome`/`ResumePermissionParams`/`turn/suspended`/`turn/resumed`。`session/resume_permission` method 名不存在——现用 `engine/resume_permission`(handlers.rs:64)，其 handler 本地 `ResumePermissionParams{request_id, outcome: PermissionOutcome}`(handlers.rs:135-137) 直接吃 PermissionOutcome(故 Defer 不能再来一次纯靠类型无法保证)。

(2) 父子 reducer 双调**完全不存在**。子 turn 经 `run_child_turn_and_deliver`→`run_turn`(subagent_spawn.rs:389) 跑，其 tool-call 经 `resolve_tool_permission` 用**子自己的 narrowed scope**(turn.rs:505-518 传 `&scope`) 单次 fold(tool_dispatch/mod.rs:488 `evaluate(scope,&decisions)`)，决策**不上报父**。ThreadHandle **无 `subagent_decision_tx`** 字段(thread.rs:29-76 只有 `subagent_final_tx`)。`SubagentFinalEvent` 仅 Completed/Errored(subagent.rs:59-84)，**无 Suspended**(注释 39-41 标 TODO B8-O6)。

(3) 基础设施齐备可复用：`PendingPermissions`(permission/pending.rs)、`PermissionReducer::{enroll,wait_unbounded,resolve_by_wire_id,cancel_all}`(permission.rs:228-330)、`Submission::ResumePermission{request_id,outcome}`(submission.rs:176-181) + actor dispatch(inner.rs:367-375) + `resume_permission`(inner.rs:488-520) 已全通；`spawn_subagent_awaitable` 返回 `Receiver<SubagentFinalEvent>`(subagent_spawn.rs:96-224)，`EngineSubagentSpawner::spawn_and_await` 现 `rx.recv()` 仅 match Completed/Errored(subagent_spawn.rs:539-547)。

## harnessRef
- B6 §4.1-4.4 (plans/.../deliverables/B6-permission-reducer.md:241-385) 锁定 defer 复用 `permission/request` 二轮交互 + pending_approvals lifecycle 表(:356-385) + wire schema 草图(:288-353: RequestPermissionOutcome::Defer / ResumePermissionParams / ResumeOutcome / TurnSuspended/Resumed)。
- B6 §3.1-3.2 (:138-227) 父子调用图 + `subagent_decision_tx: mpsc::Sender<PermissionDecision>` in-process 传值 + 短路条件(fold_child∈{Deny,Defer}不通知父)。
- B6 §7 TODO B6-O6 (:498) child Defer ⇒ 父子两层 suspended 传导。
- B8 §2.3 (plans/.../B8-subagent.md:83-110) subagent_decision_tx/subagent_final_tx 双 channel 草图 + B8-1(:421) 建议保留分离。B8 §5.3 (:284-310) `SubagentFinalEvent::Suspended{child_tid,child_request_id}`。B8 §7 (:403) 落地点。
- A3 §7.1/§7.4 (plans/.../A3-...md:457-462,556-576) 三不变式 + reducer 双调把 child 视为「一个额外 hook」append 到父 decisions 末尾。A3 §6.3(:437-451) pending reverse-request abort 走 Cancelled。
- decision-diffs.md §1.5/§1.6/§3.1/§3.2 (:140-175,424-436): Cancelled 硬约束 + Defer variant + session/resume_permission + turn/suspended + turn/resumed 全部已是「✅采纳」wire 新增项。
- pi harness 无显式 suspend/resume 原语(~/Desktop/code/github/pi/packages/agent/src/harness/ 无对应)；defer/suspend 是 zhive 特有,对齐 Claude Code defer + ACP cancelled,故不照抄 pi。

## approach
分三件协同，按依赖顺序 (a)proto → (b)父子双调 → (c)Suspended。

【(a) proto wire,zhive-proto/src/permission.rs】1. `PermissionOutcome` 加 `Defer{reason:Option<String>}` variant(:465 enum #[non_exhaustive] 已有,加 variant 不 breaking),序列化 `{"outcome":"defer",...}`，让客户端首轮回 Defer 占位。2. 新增 `ResumeOutcome{Selected{option_id},Cancelled}`——类型级禁止再 Defer(B6 §4.3 防无限挂起,:319/482)，impl From<ResumeOutcome> for PermissionOutcome。3. 新增 `ResumePermissionParams{request_id:String,outcome:ResumeOutcome}`(取代 handlers.rs:135 本地版)。4. 新增 `TurnSuspendedNotification{thread_id,turn_id,request_id:String,reason:Option<String>,suspended_at:i64}` + `TurnResumedNotification{thread_id,turn_id,resumed_at:i64}`(:331-353)。5. method 字符串常量 + notification 走现有 events/ 命名空间(见 crossModuleDeps)。

【(b) 父子 reducer 双调】否决「子 engine 实例」(B8 §2.1 已锁同 EngineInner)。采用 B6 §3.2: ThreadHandle 加 `subagent_decision_tx:Option<mpsc::Sender<SubagentDecision>>`(child 持 sender,parent 持 rx)。**关键设计收敛(本阶段范围裁剪)**: 当前子 tool-call 在子 turn task 内同步执行、父在 spawn_and_await 阻塞 rx.recv()。真正「每个子 tool-call 实时双调」需父子 turn task 间逐 tool-call 往返握手,工程量大且与现有「子 run_turn 自洽跑完」架构冲突。**建议本阶段做轻量双调**:子 dispatch 决策为非 Allow 时经 subagent_decision_tx 上报 `SubagentDecision{tool_use_id,decision}` 供父侧二次否决(父只能更严:子 Allow→父可 Deny;子 Deny→恒 Deny 短路不上报)。完整逐调握手列 follow-up。

【(c) SubagentFinalEvent::Suspended + 父子两层 suspend-resume】1. SubagentFinalEvent 加 `Suspended{child_thread_id,child_request_id:String}`(:59)。2. 子 dispatch 进入 Defer(tool_dispatch/mod.rs:592 is_defer 分支) 时,在 wait_unbounded **之前**经 subagent_final_tx 发 Suspended。3. spawn_and_await(:539) 加 Suspended 分支:父也不返回 final,父侧发 TurnSuspended(父 turn_id+子 request_id);客户端用同一 request_id 调 session/resume_permission 续命子 pending→子 wait_unbounded 解出→子续→子 final 回父→父发 TurnResumed。4. server/events.rs 加 TurnSuspended/TurnResumed 映射,engine event.rs 加对应 EngineEvent;顶层 turn 的 is_defer 也走 events_tx.send(TurnSuspended)。关键:单 EngineInner 共享一个 reducer/PendingPermissions(permission.rs:124 Arc),子的 request_id 全局唯一,resolve_by_wire_id 直接命中——无需子专属 pending map。

## files

- `crates/zhive-proto/src/permission.rs` — PermissionOutcome 加 Defer{reason:Option<String>} variant(:465);新增 ResumeOutcome{Selected{option_id:String},Cancelled} + impl From<ResumeOutcome> for PermissionOutcome;新增 ResumePermissionParams{request_id:String,outcome:ResumeOutcome};新增 TurnSuspendedNotification + TurnResumedNotification;加 pub const METHOD_RESUME_PERMISSION/METHOD_TURN_SUSPENDED/METHOD_TURN_RESUMED;每个新公开 API 配 doc+doctest;加 wire round-trip 单测
- `crates/zhive-core/src/subagent.rs` — SubagentFinalEvent 加 Suspended{child_thread_id:ThreadId,child_request_id:String}(:59);新增 SubagentDecision{tool_use_id:String,decision:PermissionDecision};删 39-41 「Suspended 省略」段并补 B8-O6 落地说明 + Suspended doctest
- `crates/zhive-core/src/state/thread.rs` — ThreadHandle 加 subagent_decision_tx:Option<mpsc::Sender<SubagentDecision>>(:75 旁);新增 new_child_with_decision 返回 (Self, Receiver<SubagentFinalEvent>, Receiver<SubagentDecision>),new_child 保留转调(decision_tx=None);new_idle/with_capacity 置 None;更新 doctest
- `crates/zhive-core/src/engine/tool_dispatch/mod.rs` — is_defer 分支(:592)在 wait_unbounded 前:子发 subagent_final_tx.Suspended,顶层 inner.events_tx().send(TurnSuspended);resolve 成功(:644)后发 TurnResumed;fold 出非 Allow(:493/:692)且有 subagent_decision_tx 时上报 SubagentDecision
- `crates/zhive-core/src/engine/event.rs` — EngineEvent 加 TurnSuspended{thread_id,turn_id,request_id:PermissionRequestId,reason:Option<String>} + TurnResumed{thread_id,turn_id}(:159 旁)
- `crates/zhive-core/src/server/events.rs` — engine_event_to_notification(:268) 加 TurnSuspended→events/turn_suspended,TurnResumed→events/turn_resumed,各 payload struct(camelCase)+单测 method 名
- `crates/zhive-core/src/server/handlers.rs` — 删本地 ResumePermissionParams(:135)改用 proto;ResumePermissionHandler::handle(:247) 把 ResumeOutcome 经 From 转 PermissionOutcome;新增 session/resume_permission 别名双注册(:64)保留 engine/resume_permission
- `crates/zhive-core/src/engine/subagent_spawn.rs` — new_child 调用点(:177)适配新签名;spawn_and_await(:539) 加 Some(Suspended{..}) 分支:父转发 TurnSuspended(父 turn_id 携子 request_id)且不返回 ToolOutput 直到子 resume 后再收 Completed;多处 test 同步
- `crates/zhive-core/src/engine/inner.rs` — resume_permission(:488)成功 resolve 后若该 request 属某 turn 则发 EngineEvent::TurnResumed;若做轻量双调,EngineSubagentSpawner 父侧持 decision rx 聚合

## newTypes

- PermissionOutcome::Defer { reason: Option<String> }(proto,加 variant)
- pub enum ResumeOutcome { Selected { option_id: String }, Cancelled }(proto,#[non_exhaustive],serde tag=outcome)
- impl From<ResumeOutcome> for PermissionOutcome
- pub struct ResumePermissionParams { request_id: String, outcome: ResumeOutcome }(proto)
- pub struct TurnSuspendedNotification { thread_id: ThreadId, turn_id: TurnId, request_id: String, reason: Option<String>, suspended_at: i64 }
- pub struct TurnResumedNotification { thread_id: ThreadId, turn_id: TurnId, resumed_at: i64 }
- pub const METHOD_RESUME_PERMISSION: &str = "session/resume_permission"(+ METHOD_TURN_SUSPENDED/RESUMED)
- SubagentFinalEvent::Suspended { child_thread_id: ThreadId, child_request_id: String }(core)
- pub(crate) struct SubagentDecision { tool_use_id: String, decision: PermissionDecision }(core,轻量双调)
- ThreadHandle.subagent_decision_tx: Option<tokio::sync::mpsc::Sender<SubagentDecision>>
- EngineEvent::TurnSuspended { thread_id, turn_id, request_id: PermissionRequestId, reason: Option<String> } + EngineEvent::TurnResumed { thread_id, turn_id }
- ThreadHandle::new_child_with_decision(新函数,new_child 转调以减改动面)

## redlineImpact
无触红线。全部复用现有依赖:tokio::sync::mpsc/oneshot、thiserror、serde、schemars(feature=schema)、tracing。**不新增 crate**。**无 unsafe**。**无 unwrap/expect 非测试**——From 转换/序列化用 ?+match;poison 恢复沿用 into_inner(permission.rs:204)。**公开 API 必加 doc+doctest**:ResumeOutcome/ResumePermissionParams/TurnSuspended/TurnResumedNotification/PermissionOutcome::Defer/SubagentFinalEvent::Suspended 全部带 doctest(proto 用 serde_json round-trip,如 permission.rs:679 现有模式)。**复用现有 error**:不新增 ReducerError variant(Defer 经 PermissionOutcome 流转,resume 失败复用 UnknownRequest/Abandoned)。**新 enum/struct 加 #[non_exhaustive]** 沿用惯例。注意:ThreadHandle::new_child 不变签名(新增 new_child_with_decision)可把 test 改动降到最小——属机械适配非红线。

## crossModuleDeps

- 与 zhive-bridge-acp 耦合:outcome_to_engine(bridge-acp/src/permission.rs:112) 把 ACP RequestPermissionOutcome→PermissionOutcome;ACP 0.12 无 Defer,bridge 的 `_ => Cancelled` 兜底(:119)仍编译通过无需改。当前只 ACP→engine 单向(不反向序列化 PermissionOutcome),安全。需知会 ACP topic owner:Defer 是 zhive 私有 outcome,bridge 不暴露。
- 与 server transport(handlers.rs)耦合:method 名分歧待协调——deliverable 写 session/resume_permission(ACP 风格),现有是 engine/resume_permission。建议新增 session/resume_permission 为正式名 + engine/resume_permission 保留别名(双注册 handlers.rs:64);notification 走现有 events/ 命名空间(events/turn_suspended)而非 turn/suspended,与 server/events.rs 全 events/ 前缀一致(B6 的 turn/suspended 是逻辑名)。需 transport/B4 owner 确认。
- 与 run_turn(turn.rs)耦合:serial resolve 循环(:505)是 Defer 挂起处;TurnSuspended 在 tool_dispatch 内发;run_turn cancel 检查(:522)对 Suspended-then-cancelled 已被 cancel_all 覆盖(permission.rs:326),无需新增清理。
- 与 B7 cancel-streaming 耦合:session/cancel→cancel_all 把 suspended Defer pending 解为 Cancelled(已实现 permission.rs:326-330 + tool_dispatch:692),Suspended→Cancelled 无需新增。
- 单 EngineInner 共享一个 reducer/PendingPermissions(permission.rs:124 Arc),子 request_id 全局唯一,父子两层 resume 都用同一 resolve_by_wire_id 命中——这是两层 suspend-resume 能成立的关键且无需子专属 map。

## tests

- proto doctest:PermissionOutcome::Defer 序列化为 {outcome:defer,reason:...};ResumeOutcome 仅 selected/cancelled 可反序列化(给 {outcome:defer} 应 Err,证明类型级禁 re-defer);From<ResumeOutcome> 映射正确;TurnSuspended/Resumed round-trip camelCase。
- proto 单测:ResumePermissionParams 反序列化 {requestId:perm:1,outcome:{outcome:selected,optionId:allow_once}}。
- core 单测:SubagentFinalEvent::Suspended 构造+match(subagent.rs doctest)。
- core 单测:tool_dispatch Defer 路径——hook 返 Defer,断言 events_tx 收到 TurnSuspended(顶层)或 subagent_final_tx 收到 Suspended(子);resolve(Selected)→续 dispatch→Approved + events_tx 收 TurnResumed。
- core 单测:子 Defer→父转发——spawn_and_await 收 Suspended 后父发 TurnSuspended(携子 request_id),resume 该 id→子续→父收 Completed→父发 TurnResumed(两层传导)。
- server/events 单测:TurnSuspended→events/turn_suspended,TurnResumed→events/turn_resumed method 名断言(对齐 :471/:486)。
- handlers 单测:engine/resume_permission + session/resume_permission 别名都路由同 handler;Defer 经 ResumeOutcome 无法构造(编译期保证)。
- 回归:wait_unbounded_receives_cancelled_on_cancel_all(permission.rs:422) 等保持绿。

## risks
1. 父子逐 tool-call 实时双调与现架构(子 run_turn 自洽跑完、父 spawn_and_await 阻塞)冲突——强行做完整握手需重构子 turn task 为「逐 tool-call 暂停等父」,工程量大、易引死锁(父等子 final、子等父 decision)。本方案降级轻量双调(子非 Allow 决策上报父二次否决,不阻塞子逐调)规避;完整版列 follow-up。
2. ThreadHandle::new_child 若改签名波及大量 test(subagent_spawn.rs:628/662/702/783/821 等)——故建议加 new_child_with_decision 新函数、new_child 转调(decision_tx=None),减改动面。
3. 子 Defer 两层 resume 路由:客户端用「子 request_id」resume,该 pending 注册在父 engine 共享的 PendingPermissions,request_id 全局唯一、resolve_by_wire_id 直接命中——已验证可行(单 reducer),无需子专属 pending map,无风险。
4. method 命名空间分歧(session/ vs engine/ vs events/)需 transport owner 拍板,否则 client/bridge 对不上。
5. Defer 无 server 超时(wait_unbounded 永等)——客户端永不 resume 且不 cancel 则 turn 永挂。B6 §4.2 接受此语义,需文档明示 + 依赖 session/cancel 兜底。

## recommendation
实现顺序:① proto 层(PermissionOutcome::Defer + ResumeOutcome + ResumePermissionParams + 两 Notification + method 常量)——零依赖,先落地解锁下游;② SubagentFinalEvent::Suspended + EngineEvent::TurnSuspended/Resumed + server/events 映射 + handlers 适配 ResumeOutcome——把「单层(顶层 turn)defer 可观测」打通,最高价值最低风险,客户端立即能区分挂起/挂死;③ 子→父 Suspended 转发(spawn_and_await 加 Suspended 分支 + 父转发 TurnSuspended)——两层 suspend-resume,中等复杂;④ **父子 reducer 双调本阶段只做轻量版**(subagent_decision_tx 上报子非 Allow 决策供父二次否决),完整逐 tool-call 握手**不在本阶段做**,作为 follow-up issue 单列(需重构子 turn task 协议)。

范围裁剪理由:(a)proto 和 (c)Suspended 是「可观测性+类型正确性」缺口,收益大、改动可控,本阶段全做;(b)完整父子双调是「分布式握手」级重构,与现有「子自洽 run_turn」架构正交冲突,轻量版已覆盖 A3 安全语义(子不能放大父权限——已由 narrowed_into + 子 scope 在 tool_dispatch:354 scope.permits 强制,这是真正 teeth),完整实时双调边际收益(父对子每步实时干预)在 Phase 1 不必要。建议本阶段 ①②③+④轻量,完整双调 defer 到后续。
