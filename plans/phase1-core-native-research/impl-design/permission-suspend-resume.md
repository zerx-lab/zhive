# Permission Defer wire + 父子 reducer 双调 + Subagent Suspended

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

【(b) 父子 reducer 双调】否决「子 engine 实例」(B8 §2.1 已锁同 EngineInner)。采用 B6 §3.2: ThreadHandle 加 `subagent_decision_tx:Option<mpsc::Sender<SubagentDecisionRequest>>`(child 持 sender,parent 持 rx)。逐 tool-call 实时双调:子 dispatch fold 出非 Deny 决策时,经 subagent_decision_tx 发 `SubagentDecisionRequest{tool_use_id,tool_name,raw_args,child_decision,reply}` 并 park 在 `reply` oneshot 上等父侧裁决;父侧 `parent_second_fold` 用 child 的 tool context 重跑父自己的 PreToolUse hooks,把 child_decision append 到父 decisions 末尾(A3 §7.4),再 evaluate 父 scope——单调:父只能等同或收紧 child,不能放大(子 Deny→恒 Deny 短路不上报)。父侧出 Ask/Defer 时经父的 reverse-RPC 就地解出(Defer 走 wait_unbounded 并先发 TurnSuspended),回给 child 的 `ParentVerdict` 恒为终态 Allow/Deny——类型级保证 child 不会再进自己的一轮 RPC。

【(c) Suspended 可观测性 + 父子两层 suspend-resume】1. 子 turn 的 tool-call Defer 由父的 reverse-RPC 就地承担:`parent_reverse_rpc` 在父 thread 上 enroll request、在父 turn 上发 `TurnSuspended`(携全局唯一 request_id)、wait_unbounded,因此 child 本身不发终态 `Suspended`。`SubagentFinalEvent::Suspended{child_thread_id,child_request_id:String}` variant 保留但当前不构造,为「child 能独立于父挂起」的后续架构预留(spawner 已留转发路径,见 spawn_and_await,触达时打 warn 而非静默丢)。2. 客户端用同一 request_id 调 `session/resume_permission`,解出父的 wait_unbounded→`ParentVerdict` 回 child→child 续→父发 `TurnResumed`。3. server/events.rs 加 TurnSuspended/TurnResumed 映射,engine event.rs 加对应 EngineEvent;顶层 turn 的 is_defer 同样走 events_tx.send(TurnSuspended)。关键:单 EngineInner 共享一个 reducer/PendingPermissions(Arc),request_id 全局唯一,resolve_by_wire_id 直接命中——无需子专属 pending map。

## files

- `crates/zhive-proto/src/permission.rs` — PermissionOutcome 加 Defer{reason:Option<String>} variant(:465);新增 ResumeOutcome{Selected{option_id:String},Cancelled} + impl From<ResumeOutcome> for PermissionOutcome;新增 ResumePermissionParams{request_id:String,outcome:ResumeOutcome};新增 TurnSuspendedNotification + TurnResumedNotification;加 pub const METHOD_RESUME_PERMISSION/METHOD_TURN_SUSPENDED/METHOD_TURN_RESUMED;每个新公开 API 配 doc+doctest;加 wire round-trip 单测
- `crates/zhive-core/src/subagent.rs` — SubagentFinalEvent 加 Suspended{child_thread_id:ThreadId,child_request_id:String}(保留但当前不构造,doc 说明为后续独立挂起架构预留 + B8-O6 落地说明);新增 ParentVerdict{Allow,Deny}(非穷尽)+ SubagentDecisionRequest{tool_use_id:String,tool_name:String,raw_args:serde_json::Value,child_decision:PermissionDecision,reply:oneshot::Sender<ParentVerdict>}(含 reply 故非 Clone,手写 Debug 跳过)
- `crates/zhive-core/src/state/thread.rs` — ThreadHandle 加 subagent_decision_tx:Option<mpsc::Sender<SubagentDecisionRequest>>(:75 旁);新增 new_child_with_decision 返回 (Self, Receiver<SubagentFinalEvent>, Receiver<SubagentDecisionRequest>),new_child 保留转调(decision_tx=None);new_idle/with_capacity 置 None;更新 doctest
- `crates/zhive-core/src/engine/tool_dispatch/mod.rs` — is_defer 分支(:592)在 wait_unbounded 前:顶层 inner.events_tx().send(TurnSuspended);resolve 成功(:644)后发 TurnResumed;子 fold 出非 Deny(:493/:692)且有 subagent_decision_tx 时发 SubagentDecisionRequest 并 await reply,按 ParentVerdict 执行/拒绝
- `crates/zhive-core/src/engine/event.rs` — EngineEvent 加 TurnSuspended{thread_id,turn_id,request_id:PermissionRequestId,reason:Option<String>} + TurnResumed{thread_id,turn_id}(:159 旁)
- `crates/zhive-core/src/server/events.rs` — engine_event_to_notification(:268) 加 TurnSuspended→events/turn_suspended,TurnResumed→events/turn_resumed,各 payload struct(camelCase)+单测 method 名
- `crates/zhive-core/src/server/handlers.rs` — 删本地 ResumePermissionParams(:135)改用 proto;ResumePermissionHandler::handle(:247) 把 ResumeOutcome 经 From 转 PermissionOutcome;新增 session/resume_permission 别名双注册(:64)保留 engine/resume_permission
- `crates/zhive-core/src/engine/subagent_spawn.rs` — new_child 调用点(:177)适配新签名;spawn_and_await(:539) 加 Some(Suspended{..}) 分支:父转发 TurnSuspended(父 turn_id 携子 request_id)且不返回 ToolOutput 直到子 resume 后再收 Completed;多处 test 同步
- `crates/zhive-core/src/engine/inner.rs` — resume_permission(:488)成功 resolve 后若该 request 属某 turn 则发 EngineEvent::TurnResumed;EngineSubagentSpawner 父侧持 decision rx,逐 SubagentDecisionRequest 跑 parent_second_fold 回 ParentVerdict

## newTypes

- PermissionOutcome::Defer { reason: Option<String> }(proto,加 variant)
- pub enum ResumeOutcome { Selected { option_id: String }, Cancelled }(proto,#[non_exhaustive],serde tag=outcome)
- impl From<ResumeOutcome> for PermissionOutcome
- pub struct ResumePermissionParams { request_id: String, outcome: ResumeOutcome }(proto)
- pub struct TurnSuspendedNotification { thread_id: ThreadId, turn_id: TurnId, request_id: String, reason: Option<String>, suspended_at: i64 }
- pub struct TurnResumedNotification { thread_id: ThreadId, turn_id: TurnId, resumed_at: i64 }
- pub const METHOD_RESUME_PERMISSION: &str = "session/resume_permission"(+ METHOD_TURN_SUSPENDED/RESUMED)
- SubagentFinalEvent::Suspended { child_thread_id: ThreadId, child_request_id: String }(core)
- pub(crate) enum ParentVerdict { Allow, Deny }(core,#[non_exhaustive],父侧二次 fold 终态裁决)
- pub(crate) struct SubagentDecisionRequest { tool_use_id: String, tool_name: String, raw_args: serde_json::Value, child_decision: PermissionDecision, reply: tokio::sync::oneshot::Sender<ParentVerdict> }(core,含 reply 故非 Clone,手写 Debug 跳过)
- ThreadHandle.subagent_decision_tx: Option<tokio::sync::mpsc::Sender<SubagentDecisionRequest>>
- EngineEvent::TurnSuspended { thread_id, turn_id, request_id: PermissionRequestId, reason: Option<String> } + EngineEvent::TurnResumed { thread_id, turn_id }
- ThreadHandle::new_child_with_decision(新函数,new_child 转调以减改动面)

## redlineImpact
无触红线。全部复用现有依赖:tokio::sync::mpsc/oneshot、thiserror、serde、schemars(feature=schema)、tracing。**不新增 crate**。**无 unsafe**。**无 unwrap/expect 非测试**——From 转换/序列化用 ?+match;poison 恢复沿用 into_inner(permission.rs:204)。**公开 API 必加 doc+doctest**:ResumeOutcome/ResumePermissionParams/TurnSuspended/TurnResumedNotification/PermissionOutcome::Defer/SubagentFinalEvent::Suspended 全部带 doctest(proto 用 serde_json round-trip,如 permission.rs:679 现有模式)。**复用现有 error**:不新增 ReducerError variant(Defer 经 PermissionOutcome 流转,resume 失败复用 UnknownRequest/Abandoned)。**新 enum/struct 加 #[non_exhaustive]** 沿用惯例。注意:ThreadHandle::new_child 不变签名(新增 new_child_with_decision)可把 test 改动降到最小——属机械适配非红线。

## crossModuleDeps

- 与 zhive-bridge-acp 耦合:outcome_to_engine(bridge-acp/src/permission.rs:112) 把 ACP RequestPermissionOutcome→PermissionOutcome;ACP 0.13 无 Defer,bridge 的 `_ => Cancelled` 兜底(:119)仍编译通过无需改。当前只 ACP→engine 单向(不反向序列化 PermissionOutcome),安全。Defer 是 zhive 私有 outcome,bridge 不暴露。
- 与 server transport(handlers.rs)耦合:`session/resume_permission`(METHOD_RESUME_PERMISSION)为正式名,`engine/resume_permission`(METHOD_RESUME_PERMISSION_LEGACY)保留别名,二者双注册路由同一 handler;notification 走现有 events/ 命名空间(events/turn_suspended / events/turn_resumed)而非 turn/suspended,与 server/events.rs 全 events/ 前缀一致(B6 的 turn/suspended 是逻辑名)。
- 与 run_turn(turn.rs)耦合:serial resolve 循环(:505)是 Defer 挂起处;TurnSuspended 在 tool_dispatch 内发;run_turn cancel 检查(:522)对 Suspended-then-cancelled 已被 cancel_all 覆盖(permission.rs:326),无需新增清理。
- 与 B7 cancel-streaming 耦合:session/cancel→cancel_all 把 suspended Defer pending 解为 Cancelled(已实现 permission.rs:326-330 + tool_dispatch:692),Suspended→Cancelled 无需新增。
- 单 EngineInner 共享一个 reducer/PendingPermissions(permission.rs:124 Arc),子 request_id 全局唯一,父子两层 resume 都用同一 resolve_by_wire_id 命中——这是两层 suspend-resume 能成立的关键且无需子专属 map。

## tests

- proto doctest:PermissionOutcome::Defer 序列化为 {outcome:defer,reason:...};ResumeOutcome 仅 selected/cancelled 可反序列化(给 {outcome:defer} 应 Err,证明类型级禁 re-defer);From<ResumeOutcome> 映射正确;TurnSuspended/Resumed round-trip camelCase。
- proto 单测:ResumePermissionParams 反序列化 {requestId:perm:1,outcome:{outcome:selected,optionId:allow_once}}。
- core 单测:SubagentFinalEvent::Suspended 构造+match(subagent.rs doctest)。
- core 单测:tool_dispatch Defer 路径——hook 返 Defer,断言 events_tx 收到 TurnSuspended(顶层 turn);resolve(Selected)→续 dispatch→Approved + events_tx 收 TurnResumed。
- core 单测:子 Defer→父承担——子发 SubagentDecisionRequest,父 parent_second_fold 出 Defer 时经 parent_reverse_rpc 在父 turn 发 TurnSuspended(携全局唯一 request_id),resume 该 id→父 wait_unbounded 解出→ParentVerdict 回子→子续→父发 TurnResumed。
- server/events 单测:TurnSuspended→events/turn_suspended,TurnResumed→events/turn_resumed method 名断言(对齐 :471/:486)。
- handlers 单测:engine/resume_permission + session/resume_permission 别名都路由同 handler;Defer 经 ResumeOutcome 无法构造(编译期保证)。
- 回归:wait_unbounded_receives_cancelled_on_cancel_all(permission.rs:422) 等保持绿。

## risks
1. 父子逐 tool-call 实时双调易引死锁(父等子 final、子等父 decision)——故 reverse-RPC 的解出由 engine actor loop 驱动而非子/父 task 自身,且回给子的 ParentVerdict 恒为终态 Allow/Deny(父出 Ask/Defer 在父 reverse-RPC 内就地解出),类型级保证子不会再进自己的一轮 RPC,断开循环等待。
2. ThreadHandle::new_child 若改签名波及大量 test(subagent_spawn.rs:628/662/702/783/821 等)——故建议加 new_child_with_decision 新函数、new_child 转调(decision_tx=None),减改动面。
3. 子 Defer 两层 resume 路由:客户端用「子 request_id」resume,该 pending 注册在父 engine 共享的 PendingPermissions,request_id 全局唯一、resolve_by_wire_id 直接命中——已验证可行(单 reducer),无需子专属 pending map,无风险。
4. method 命名空间:request 用 session/resume_permission(engine/resume_permission 别名),notification 用 events/turn_suspended、events/turn_resumed,与 server/events.rs 全 events/ 前缀一致。
5. Defer 无 server 超时(wait_unbounded 永等)——客户端永不 resume 且不 cancel 则 turn 永挂。B6 §4.2 接受此语义,需文档明示 + 依赖 session/cancel 兜底。

## recommendation
实现顺序:① proto 层(PermissionOutcome::Defer + ResumeOutcome + ResumePermissionParams + 两 Notification + method 常量)——零依赖,先落地解锁下游;② EngineEvent::TurnSuspended/Resumed + server/events 映射 + handlers 适配 ResumeOutcome——把「单层(顶层 turn)defer 可观测」打通,客户端立即能区分挂起/挂死;③ 父子 reducer 逐 tool-call 实时双调(subagent_decision_tx 发 SubagentDecisionRequest,父 parent_second_fold 跑父 PreToolUse hooks + append child decision + evaluate 父 scope,reverse-RPC 就地解 Ask/Defer 回 ParentVerdict);④ SubagentFinalEvent::Suspended variant 保留但不构造,为「child 独立挂起」后续架构预留(spawner 已留转发路径)。

A3 安全语义:子不能放大父权限——由 narrowed_into + 子 scope 在 dispatch 处 scope.permits 强制,这是真正 teeth;父侧二次 fold 单调,只能等同或收紧 child 决策。reverse-RPC 解出由 engine actor loop 驱动、ParentVerdict 恒终态,断开父子循环等待。
