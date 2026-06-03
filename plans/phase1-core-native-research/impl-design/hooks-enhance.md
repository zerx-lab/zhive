# Hooks 增强：Unknown deserialize 降级 + 每-hook timeout 隔离 + Subprocess 双轨 + signal/CancellableHook

## currentState
四件全部未实装，现状精确如下：
(a) Unknown：crates/zhive-proto/src/hook.rs:175-214 `HookEvent` 用 `#[derive(Serialize,Deserialize)]` + `#[serde(tag="hook_event_name")]` + `#[non_exhaustive]`，**无 Unknown variant**；模块顶注释 hook.rs:17-24 明说"Unknown 推给 B5 手写 Deserialize"。下游已为非穷尽预留兜底：crates/zhive-core/src/hooks/mod.rs:327-347 `fn hook_event_name` 末尾 `_ => "Unknown"`，mod.rs:357-377 `fn hook_session_id` 末尾 `_ => ""`，crates/zhive-core/src/engine/compaction.rs:209-212 `_ => "manual"`。当前 internally-tagged derive 遇未知 tag 直接 `Err`（serde "unknown variant"），调用点 tool_dispatch/mod.rs:413 与 compaction.rs:223 都会把 Err 当 dispatch 失败处理（前者 treat as Deny，后者 warn 继续）。
(b) timeout：crates/zhive-core/src/hooks/mod.rs:277-317 `dispatch_inner` 串行 for-loop，每个 callback 仅 `std::panic::AssertUnwindSafe(callback.call(event)).catch_unwind().await`（mod.rs:292-293），**无 tokio::time::timeout 包裹**；HookRegistration（mod.rs:102-113）无 timeout 字段。
(c) Subprocess：HookRegistration.callback 字段是裸 `Arc<dyn HookFn>`（mod.rs:112），**无 HookExecutor enum**，无 InProcess/Subprocess 双轨。
(d) signal：crates/zhive-core/src/hooks/mod.rs:30-37 `trait HookFn { async fn call(&self, event:&HookEvent)->Option<HookOutput> }` **无第三参 signal**；dispatch 也不接收 CancellationToken。外部实现者：crates/zhive-bridge-acp/tests/conformance.rs:28 与 crates/zhive-core/src/engine.rs:1434 `impl HookFn for FixedDecisionHook`（测试桩）。dispatch 调用点仅两处：tool_dispatch/mod.rs:413、compaction.rs:223。

## harnessRef
B7 §6.4 hook signature 双 trait + blanket adapter：/home/zero/Desktop/code/zerx-lab/zhive/plans/phase1-core-native-research/deliverables/B7-cancel-streaming.md:462-498（Hook / CancellableHook / HookAdapter blanket impl 蓝本），§6.3 表 B7:450-458（按 emit 路径注入 token），§6.5 失败模式 B7:500-507（非 cancellable 长跑兜底）。A4 §2-Q1 Unknown 手写 Deserialize 思路：deliverables/A4-hook-event-schema.md:113-129（先反序列化成 Value 按 tag 分发），§5 策略表 A4:777-797（方案 C 保留 raw payload，方案 B `#[serde(other)]` unit 丢 payload 被否）。B5 §0 双轨 + §3.4 隔离表：deliverables/B5-hook-host.md:30（HookExecutor InProcess/Subprocess 双轨，Phase 1 仅 InProcess）、B5:443-454（timeout/panic/cancel 隔离表）、B5:838（Q2 subprocess stdin JSON/stdout JSON）。Pi 锚点：${PI}/packages/agent/docs/hooks.md:21-32（signal? 可选）、agent-harness.ts:701/759（一次性 signal）。Claude Code subprocess hook：stdin 喂 JSON event / stdout 收 JSON output / exit-code 语义（B5:908 TODO B5-9 占位，Phase 2 才落协议）。决策已采纳：decision-diffs.md:55（3.5 保留 Subprocess variant）、decision-diffs.md:402（futures.catch_unwind workspace 已有）、decision-diffs.md:465（双轨 Phase 1 仅 InProcess）。

## approach
分四件，按依赖顺序：

(a) Unknown 手写 Deserialize（zhive-proto）。选：保留 `#[derive(Serialize)]` + `#[serde(tag="hook_event_name")]`（Serialize 不动），把 `#[derive(Deserialize)]` 从 HookEvent 上移除改手写 impl。手写体：先 `let v = serde_json::Value::deserialize(d)?`，读 `v["hook_event_name"]` 字符串；match 14+1 已知 tag → `serde_json::from_value::<XxxInput>(v).map(HookEvent::Xxx)`；命中未知 tag 或缺 tag → `HookEvent::Unknown { name, payload: v }`。新增 `Unknown { name:String, payload:Value }` variant，打 `#[serde(skip)]` 让派生 Serialize 不处理它（Unknown 序列化另走手写或 §5 约定"只发不收"——按 A4:797 Unknown 不可订阅，dispatch 不会产生它，故 Serialize 端 Unknown 用手写 Serialize 直接 emit `payload` 原 object 即可，避免 derive panic）。**否决** `#[serde(other)]`：A4:786 限制 unit variant 丢 payload。**否决** untagged fallback：A4:788 O(n)+误匹配。schema feature 下 `#[cfg_attr(feature="schema",derive(JsonSchema))]` 在手写 Deserialize 时保留（JsonSchema 与 Deserialize 解耦，仍可 derive）；但 Unknown variant 对 schemars 用 `#[cfg_attr(feature="schema",schemars(skip))]` 避免 Value 污染 schema。

(b) 每-hook timeout（zhive-core/hooks/mod.rs）。HookRegistration 加 `timeout: Option<Duration>` 字段（None=不限时，保持现有测试桩零改动语义）；register() 加 timeout 参数或新增 `register_with_timeout`（见下 cross-module，倾向给 register 加参数会破多处调用，故**新增 builder/重载**避免雪崩）。dispatch_inner 内把 `fut.catch_unwind()` 再包一层：`match hook.timeout { Some(d)=>tokio::time::timeout(d, fut).await, None=>Ok(fut.await) }`，超时降级语义=与 panic 一致（warn + skip 该 hook、不贡献 HookOutput、继续下一个，对齐 B5:449/B7:504）。tokio time feature 已在 workspace（Cargo.toml:47）。

(c) Subprocess 双轨（zhive-core/hooks/mod.rs）。引入 `enum HookExecutor { InProcess(Arc<dyn HookFn>), Subprocess(SubprocessSpec) }`，HookRegistration.callback 改 `executor: HookExecutor`。Phase 1 仅实装 InProcess 执行路径；Subprocess variant 加 `#[allow(dead_code)]` 占位 SubprocessSpec{ program:String, args:Vec<String> }，dispatch 命中 Subprocess 时返回/记录 `tracing::warn` 并 skip（Phase 1 不 spawn，对齐 B5:838 仅占位）。**安全考量**写进 doc：子进程协议（stdin JSON / stdout JSON / exit-code）Phase 2 才定（B5:908），Phase 1 不允许注册 Subprocess（register 侧拒绝或仅 builtin loader 可构造）。为最小化爆炸面：保留现有 `register(...callback: Arc<dyn HookFn>)` 签名内部包成 `HookExecutor::InProcess`，对外零破坏。

(d) signal / CancellableHook（zhive-core/hooks/mod.rs）。按 B7 §6.4 双 trait：保留现 `trait HookFn`（无 signal，所有现有实现零改动）；新增 `trait CancellableHookFn: Send+Sync { async fn call(&self, event:&HookEvent, signal:&CancellationToken)->Option<HookOutput> }`；blanket `struct HookFnAdapter<H>(Arc<H>)` 或直接对 `Arc<dyn HookFn>` 实现 CancellableHookFn（忽略 signal）。dispatch 增 `dispatch_with_signal(&self, event, signal:&CancellationToken)`；旧 `dispatch(event)` 内部用 `CancellationToken::new()`（never-cancelled 哨兵，等价 Pi 一次性 signal，B7:455 对比改进点保留）调用 with_signal 版，**保持现有两调用点零改动**。dispatch 循环把 signal 透传给 callback，并用 `tokio::select!{ _=signal.cancelled()=>break, r=（timeout 包裹的 fut）=>... }`——cancel 短路整个 dispatch（对齐 B5:450 cancel 是唯一打断 dispatch 的信号）。HookFn 第三参不强加（避免破坏 conformance.rs/engine.rs 测试桩），通过 adapter 抬升。

## files

- `crates/zhive-proto/src/hook.rs` — 移除 HookEvent 上的 #[derive(Deserialize)]（保留 Serialize/Debug/Clone/PartialEq + cfg schema JsonSchema）；新增 variant Unknown{name:String,payload:serde_json::Value}（schemars skip）；手写 impl<'de> Deserialize<'de> for HookEvent（Value::deserialize → 读 hook_event_name → match 15 known tag from_value，未知/缺失落 Unknown）；为 Unknown 的 Serialize 正确性手写 impl Serialize for HookEvent 或在 Unknown 上 #[serde(skip)]+约定不发；补 doctest：未知 tag round-trips 成 Unknown，已知 tag 不受影响；补 #[cfg(test)] 测试 unknown_tag_falls_back / missing_tag_falls_back / known_tag_still_typed
- `crates/zhive-core/src/hooks/mod.rs` — HookRegistration 加 timeout:Option<Duration> + 把 callback:Arc<dyn HookFn> 改为 executor:HookExecutor；新增 enum HookExecutor{InProcess(Arc<dyn HookFn>),Subprocess(SubprocessSpec)} 与 struct SubprocessSpec{program,args}（dead_code 占位）；新增 trait CancellableHookFn + 对 Arc<dyn HookFn> 的 blanket adapter（忽略 signal）；HookHost::register 内部包 InProcess+timeout=None 保持旧签名零破坏，另加 register_with(timeout) 入口；dispatch_inner 重写为 dispatch_with_signal：tokio::select! cancel 短路 + 每 hook tokio::time::timeout 包裹 catch_unwind，timeout/panic 都 warn+skip+continue；旧 dispatch(event) 用 never-cancelled 哨兵 token 委派 with_signal 版；补 timeout_isolated / signal_short_circuits / subprocess_skipped_phase1 测试
- `crates/zhive-core/src/hooks/mod.rs (fn hook_event_name / hook_session_id)` — 两 fn 增加 HookEvent::Unknown 显式 arm（name 返回 "Unknown"+保留 payload 内 sessionId 探测 or 空串），保留末尾 _ 兜底；现有 _ => 已覆盖，可只验证不破坏
- `crates/zhive-core/src/engine/tool_dispatch/mod.rs` — dispatch 调用点（mod.rs:413）若要透传 turn cancel token，改调 dispatch_with_signal(&pre_event, &active_turn_cancel)；若本阶段不接线则保持 dispatch(&pre_event) 不变（向后兼容）。建议本阶段先保持不变，signal 接线随 B7 turn cancel 落地
- `crates/zhive-core/src/engine/compaction.rs` — dispatch_compact_hook 调用点（compaction.rs:223）同上，可保留 dispatch(&ev)；未来接 engine.compaction_cancel 时改 dispatch_with_signal（B7:455）

## newTypes

- crates/zhive-proto/src/hook.rs: 在 enum HookEvent 增 `Unknown { name: String, payload: serde_json::Value }`
- crates/zhive-proto/src/hook.rs: `impl<'de> serde::Deserialize<'de> for HookEvent`（手写，替换派生）
- crates/zhive-proto/src/hook.rs: 可能需 `impl serde::Serialize for HookEvent`（手写，或保留派生 + Unknown 标 skip 并约定不序列化）
- crates/zhive-core/src/hooks/mod.rs: `pub enum HookExecutor { InProcess(Arc<dyn HookFn>), Subprocess(SubprocessSpec) }`
- crates/zhive-core/src/hooks/mod.rs: `pub struct SubprocessSpec { pub program: String, pub args: Vec<String> }`（#[allow(dead_code)] Phase 1 占位）
- crates/zhive-core/src/hooks/mod.rs: `#[async_trait] pub trait CancellableHookFn: Send + Sync { async fn call(&self, event: &HookEvent, signal: &tokio_util::sync::CancellationToken) -> Option<HookOutput>; }`
- crates/zhive-core/src/hooks/mod.rs: blanket `#[async_trait] impl CancellableHookFn for Arc<dyn HookFn>`（忽略 signal）
- crates/zhive-core/src/hooks/mod.rs: `HookRegistration.timeout: Option<std::time::Duration>`
- crates/zhive-core/src/hooks/mod.rs: `pub async fn HookHost::dispatch_with_signal(&self, event: &HookEvent, signal: &CancellationToken) -> Result<Vec<HookOutput>, HookHostError>`
- crates/zhive-core/src/hooks/mod.rs: `pub fn HookHost::register_with_timeout(...) -> Result<ExtensionScope, HookHostError>`（或给现有 register 加 timeout 参数，但前者破坏面小）
- HookHostError 可加 `#[error] HookTimeout`（仅当需要把 timeout 当错误上报；按降级语义倾向不加，只 warn+skip）

## redlineImpact
无新 crate 依赖。所需均已在依赖树：
- futures::FutureExt::catch_unwind —— workspace 已有（decision-diffs.md:402，root Cargo.toml:46），zhive-core 已用（mod.rs:21）。
- tokio::time::timeout —— tokio "time" feature 已在 workspace（Cargo.toml:47），zhive-core 已依赖 tokio。
- tokio_util::sync::CancellationToken —— tokio-util default 已含 sync（B7:536），zhive-core 已依赖 tokio-util（Cargo.toml zhive-core deps）。
- async-trait —— zhive-core 已依赖（用于 CancellableHookFn）。
- zhive-proto 的 Unknown 手写 Deserialize 仅用 serde + serde_json（proto 已有 serde derive+std+serde_json），**不引入 tokio-util/futures 到 proto**（proto 无这些依赖，故 signal/timeout 必须留在 core，Unknown 必须纯 serde —— 已遵守）。
红线规避：禁止 unwrap/expect 在非测试码 —— 手写 Deserialize 用 `?` + serde::de::Error::custom/missing_field，绝不 unwrap；Value 取字段用 match/Option + ok_or。禁止 unsafe —— catch_unwind 走 std::panic::AssertUnwindSafe（已有模式，非 unsafe）。公开 API doc+doctest：Unknown variant、CancellableHookFn、HookExecutor、dispatch_with_signal 均需 doc comment + 至少一个 doctest（Unknown 反序列化 doctest 易写）。复用已有 error 类型 HookHostError/ValidatorError，不平行造轮。`#[non_exhaustive]` 已在 HookEvent/HookHostError 上 —— 加 Unknown variant 与 non_exhaustive 兼容（下游 _ 已存在）。

## crossModuleDeps

- A4(proto hook schema) ↔ B5(core hook host)：Unknown variant 落在 proto（hook.rs），手写 Deserialize 也在 proto；core 侧 fn hook_event_name(mod.rs:327)/hook_session_id(mod.rs:357) 的 _ 兜底已为 Unknown 预留，加 Unknown 后两 fn 不会 break（已验证有 _ arm）。约定：Unknown 仅反序列化产生，dispatch 不主动构造（A4:797 不可订阅），故 HookFilter::matches(mod.rs:85) 对 Unknown 走 _=>None 分支自然不匹配，无需改。
- B7(cancel-streaming) ↔ B5：signal 注入点由 B7 决定（turn cancel/compaction_cancel）。本阶段 dispatch_with_signal 提供入口但两调用点(tool_dispatch:413 / compaction:223)暂传 never-cancelled 哨兵或保持旧 dispatch()。真正接线（Some(&active_turn.cancel)）随 B7 turn-cancel 落地，避免本阶段与 B7 抢 ActiveTurn.cancel 字段所有权。
- engine.hook_host 类型：engine/inner.rs:102 `hook_host: Arc<HookHost>`、engine.rs:180 EngineConfig.hook_host、tool_dispatch/mod.rs 多签名 `&Arc<HookHost>`——只要 register 旧签名与 dispatch 旧方法保留，这些零改动。新增 dispatch_with_signal/register_with_timeout 为附加 API。
- 测试桩 impl HookFn：conformance.rs:28、engine.rs:1434 FixedDecisionHook、mod.rs 内 Counting/Panicking/Probe——HookFn trait 不加第三参，故全部零改动；新 CancellableHookFn 通过 blanket adapter 兼容，无需改测试桩。
- SubprocessSpec 与 A5 manifest entrypoint：Phase 1 entrypoint 仅 builtin（decision-diffs.md:35/267），故 manifest loader 不会构造 Subprocess；占位即可，协议留 Phase 2(B5:908)。

## tests

- proto doctest：HookEvent 反序列化未知 hook_event_name（如 "FutureEvent"）→ matches!(ev, HookEvent::Unknown{..}) 且 payload 保留原字段
- proto unit：known_tag_still_typed（Stop/PreToolUse 仍反序列化成具体 variant，不退化 Unknown）
- proto unit：missing_hook_event_name_field → Unknown（name 为空或哨兵）而非 panic
- proto unit：Unknown round-trip 行为（约定不发则测 serialize Unknown 不 panic / 输出原 payload）
- proto unit：现有 pre_tool_use_round_trip(hook.rs:652)/stop_event_tag_and_flatten 保持绿（回归）
- core unit：timeout_hook_isolated_and_continues（注册一个 sleep 超 timeout 的 hook + 一个 counting hook，dispatch 后 counting 仍跑、超时 hook 无 HookOutput、dispatch Ok）
- core unit：signal_cancel_short_circuits_dispatch（pre-cancelled token → dispatch_with_signal 立即返回、后续 hook 不跑）
- core unit：cancellable_hook_receives_signal（实现 CancellableHookFn 的 hook 能 select cancel 提前返回）
- core unit：blanket_adapter_ignores_signal（旧 HookFn 经 adapter 在 cancel 下仍正常跑完短操作）
- core unit：subprocess_executor_skipped_in_phase1（注册 Subprocess variant → dispatch warn+skip，不 spawn、不 panic）
- core 回归：现有 callback_panic_is_isolated_and_dispatch_continues(mod.rs:574)、registrations_are_inserted_sorted_by_priority(mod.rs:482)、dropping_scope_deregisters_hook(mod.rs:549) 保持绿
- core doctest：dispatch_with_signal、CancellableHookFn、HookExecutor 各一个 example

## risks
手写 Deserialize 与派生 Serialize 共存的最大坑：internally-tagged enum 派生 Serialize 会把 `hook_event_name` 注入对象；Unknown variant 在派生 Serialize 下会尝试 emit `"hook_event_name":"Unknown"` + 把 {name,payload} 当 newtype/struct，与 wire 不一致。规避：要么对 Unknown 标 `#[serde(skip)]`（约定 Unknown 永不 serialize，符合 A4:797 不可订阅/不发），要么整体手写 Serialize（更稳但代码量大）——推荐前者 + 在 Unknown doc 标注"反序列化兜底专用，序列化未定义"。
第二坑：`#[serde(flatten)]` + 手写 Deserialize 经 Value 中转——各 Input struct 内 `#[serde(flatten)] base` 在 from_value 时仍正常（Value 保留全字段），已验证 proto 大量用 flatten，无碍。
第三坑：schemars JsonSchema 派生在手写 Deserialize 后仍可 derive（解耦），但 Unknown 的 Value 字段会进 schema —— 用 schemars(skip) 排除。
第四坑：tokio::time::timeout 把 fut 的生命周期延长，catch_unwind 需在 timeout 内层（timeout(d, AssertUnwindSafe(fut).catch_unwind())），顺序写反会丢 panic 隔离。
第五坑：never-cancelled 哨兵 token 若每次 dispatch new 一个，开销可忽略（CancellationToken::new 廉价），但不要存成字段共享（语义混乱）。
低风险：CancellableHookFn 用 async-trait 与现有 HookFn 一致风格，object-safety OK（已有 Arc<dyn HookFn> 先例）。

## recommendation
实现顺序：(a)→(b)→(c)→(d)。
1. 先做 (a) Unknown（zhive-proto 独立、零跨 crate 影响、可单独 cargo check -p zhive-proto + nextest 验证），它是 A4 明确指派 B5 的 TODO，优先级最高且解耦。
2. 再做 (b) timeout（core 内、改动局部、给 HookRegistration 加 Option 字段 + register_with_timeout 新入口保旧签名零破坏）。
3. (c) Subprocess 仅做"双轨 enum 占位 + InProcess 包装 + Subprocess skip+warn"，**不实装 spawn/stdin-stdout 协议**（Phase 2，B5:908）——本阶段只把结构留好，避免过度工程。
4. (d) signal：做完整双 trait + blanket adapter + dispatch_with_signal 入口，但**两个 dispatch 调用点暂不接真实 turn cancel token**（传哨兵或保留旧 dispatch()），真实注入随 B7 turn-cancel 一起落，避免本阶段与 B7 争 ActiveTurn.cancel 所有权造成返工。
范围建议：(a)(b)(d) 本阶段做到可用+测试齐全；(c) 本阶段只到"双轨结构+Phase1 拒绝/跳过 Subprocess"，子进程执行体推 Phase 2。所有新公开 API 必须带 doc+doctest（红线）。每步独立 `cargo check -p <crate> --lib` + `cargo nextest run -p <crate>`，最后 fmt+clippy -D warnings。
