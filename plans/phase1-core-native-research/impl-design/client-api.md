# Block C：ClientBuilder + connect_remote 占位 + shutdown timeout + cancel 对齐

## harnessRef
codex app-server-client/src/lib.rs:122（SHUTDOWN_TIMEOUT=5s 常量）、lib.rs:763-795（shutdown async 实现：drop event_rx → send Shutdown cmd → timeout(5s) wait worker → timeout(5s) abort worker_handle）、lib.rs:330-368（InProcessClientStartArgs 字段：client_name/client_version/channel_capacity/experimental_api/opt_out_notification_methods）、lib.rs:485-608（start(args) 入口，握手参数由 args.initialize_params() 组装，caller 不直接碰 wire）

## approach
选定方案：**渐进式 Builder 叠加，不破坏现有 Arc-clone 模型**。

(a) ClientBuilder：在 connect.rs 同文件新增 `pub struct ClientBuilder` + `impl ClientBuilder`，五个 setter（client_info, capabilities, protocol_version, channel_capacity, initialize_timeout），两个终态方法（connect_uds, connect_stdio）均消费 builder。**现有 `Client::connect_uds` 和 `Client::connect_stdio` 保留为向后兼容的 free function，内部委托 `ClientBuilder::default().connect_uds/connect_stdio`**——调用方无感知。perform_handshake 重构为 perform_handshake_with_params(client, client_info, capabilities, protocol_version, initialize_timeout)，从 builder 注入参数替换硬编码(connect.rs:68-77)。

(b) connect_remote 占位：在 ClientBuilder 上新增 pub async fn connect_remote(self, url: String) -> Result<Client, ClientError>，方法体直接返回 Err(ClientError::NotImplemented { feature: "remote/websocket", phase: 3 })。同步在 error.rs 新增 NotImplemented 变体。**不新增任何依赖**，tokio-tungstenite 不引入。

(c) shutdown 加 5s timeout：现有 Client::shutdown(self) 为同步(lib.rs:459-462)，转为 **async fn shutdown(self) -> Result<(), ClientError>**。实现：先 cancel shutdown token，再 tokio::time::timeout(Duration::from_secs(5), worker_drain_signal).await，超时则直接 abort。**tokio features 含 time**（features=["io-std","time"]）——这是现有 feature 扩展，非新 crate，不触红线1。worker_drain_signal 通过新增 `Arc<tokio::sync::Notify>` 字段实现：reader task 退出时 notify.notify_one()，shutdown().await 等这个 Notify。保留现有 Drop impl（best-effort cancel token，不 await）。engine_host.rs:131 的 `self.client.clone().shutdown()` 会变成同步 call 编译失败——**需要更新为 `.clone().shutdown().await` 并在外层 async fn 中调用**，engine_host.rs:128 的 stop() 已是 async 故无问题。

(d) cancel_session()：**评估结论是：补 cancel_session() 通知型 helper，与现有 cancel_turn() RPC 并存**。两者语义不同：cancel_turn() 是 engine-private RPC（engine/cancel_turn 方法，回 TurnId），cancel_session() 是 ACP 标准 notification（session/cancel，无 response）。server 侧 register_engine_handlers 注册了 session/cancel 通知处理逻辑（B7），收到后映射为 engine.cancel_turn(thread_id)。因此 cancel_session() client 侧方法发出 notification 后即在 server 端生效。**本方案：在 lib.rs 的 impl Client 块里新增 cancel_session(&self, thread_id: &ThreadId)，内部调 self.notify("session/cancel", Some(json!({threadId: thread_id})))，返回 Result<(), ClientError>**。SessionId 不单独定义——根据 domain.rs 现状，zhive 中 ACP SessionId ≈ ThreadId（domain.rs:129 Thread.session_id = Option<AcpSessionId>，但 cancel 语义是 per-thread），所以 cancel_session 接受 &ThreadId，与现有 cancel_turn 签名对称。

被否决的备选：
- 不提供 Builder 直接改 connect_uds/connect_stdio 签名：会破坏 engine_host.rs:100 等调用点。
- shutdown 不加 async，用 spawn+AbortHandle：破坏 stop() async 调用链，语义更模糊。
- cancel_session 接受 &AcpSessionId：AcpSessionId 是 bridge-only 类型，不适合出现在 client API。

## files

- `crates/zhive-client-native/Cargo.toml` — 在 tokio features 列表中添加 'time'：`tokio = { workspace = true, features = ['io-std', 'time'] }`。这是现有 crate 的 feature 扩展，非新依赖。
- `crates/zhive-client-native/src/error.rs` — 在 ClientError 枚举新增变体：`#[error('not implemented in phase {phase}: {feature}')] NotImplemented { feature: &'static str, phase: u8 }`。并补相应 doctest example。
- `crates/zhive-client-native/src/connect.rs` — （1）新增 pub struct ClientBuilder { client_info: Option<Implementation>, capabilities: Capabilities, protocol_version: ProtocolVersion, channel_capacity: usize, initialize_timeout: Duration }，实现 Default + 五个 builder setter + connect_uds(path)/connect_stdio()/connect_remote(url) 三个终态 async fn。（2）重构 perform_handshake 为带参数形式 perform_handshake_with_params(client, &params)，其中 params 是从 builder 字段组装的握手 payload；硬编码的 client_info/capabilities/protocol_version(connect.rs:68-77) 替换为参数传入。（3）现有 impl Client { connect_uds / connect_stdio } 方法体改为委托 ClientBuilder::default()，保持向后兼容。（4）connect_remote 方法体：直接 return Err(ClientError::NotImplemented { feature: 'remote/websocket', phase: 3 })，doc comment 注明 Phase 3 not implemented。（5）所有公开类型/方法补 doc comment + doctest example。
- `crates/zhive-client-native/src/lib.rs` — （1）在 Inner struct 新增 pub(crate) worker_done: Arc<tokio::sync::Notify>（行81 附近）。（2）在 from_split_with_meta 构建 Inner 时初始化 worker_done Arc<Notify>，同时把它传给 transport::spawn_reader（reader 退出时调 notify_one()）。（3）Client struct 对应增加 worker_done 字段。（4）Client::shutdown(self) 改为 pub async fn shutdown(self) -> Result<(), ClientError>：先调 inner.shutdown.cancel()，再 drop(self.outbound_tx)，再 tokio::time::timeout(Duration::from_secs(5), self.inner.worker_done.notified()).await（超时则继续，不返回 err，符合 best-effort 语义，对齐 codex lib.rs:790-793）。（5）新增 pub async fn cancel_session(&self, thread_id: &ThreadId) -> Result<(), ClientError>：内部调 self.notify('session/cancel', Some(serde_json::json!({"threadId": thread_id}))).await，doc comment 说明 ACP session/cancel notification 语义（fire-and-forget，server 处理后发 session/aborted notification）、与 cancel_turn 的区别（turn_id vs session/thread 维度）、以及 server 在 register_engine_handlers 注册的 session/cancel handler（B7，映射为 engine.cancel_turn）。（6）补 cancel_session doctest。
- `crates/zhive-client-native/src/transport.rs` — 在 ReaderArgs struct 新增 pub(crate) worker_done: Arc<tokio::sync::Notify> 字段，在 spawn_reader 退出时（ordered teardown 最后一步，events_tx drop 之后）调用 worker_done.notify_one()，确保 shutdown().await 能检测到 reader 已退出。
- `crates/zhive-cli/src/engine_host.rs` — Host::stop() 中 self.client.clone().shutdown() 改为 self.client.clone().shutdown().await（stop 已是 async fn，无问题）。同时将 return 类型相关错误静默（shutdown 返回 Result 但 stop 忽略它，用 let _ =）。

## newTypes

- pub struct ClientBuilder { client_info: Option<Implementation>, capabilities: Capabilities, protocol_version: ProtocolVersion, channel_capacity: usize, initialize_timeout: Duration }
- impl ClientBuilder { pub fn new() -> Self; pub fn client_info(self, info: Implementation) -> Self; pub fn capabilities(self, caps: Capabilities) -> Self; pub fn protocol_version(self, v: ProtocolVersion) -> Self; pub fn channel_capacity(self, cap: usize) -> Self; pub fn initialize_timeout(self, d: Duration) -> Self; pub async fn connect_uds(self, path: impl AsRef<Path>) -> Result<Client, ClientError>; pub async fn connect_stdio(self) -> Result<Client, ClientError>; pub async fn connect_remote(self, url: String) -> Result<Client, ClientError>; }
- ClientError::NotImplemented { feature: &'static str, phase: u8 }
- // 新增 Client 方法：
pub async fn shutdown(self) -> Result<(), ClientError>  // 从同步改为 async，加 5s timeout
- pub async fn cancel_session(&self, thread_id: &ThreadId) -> Result<(), ClientError>
- // Inner 新增字段：
pub(crate) worker_done: Arc<tokio::sync::Notify>

## redlineImpact
触发项：tokio 'time' feature 扩展（非新 crate，现有 workspace dep 的 feature 追加）。CLAUDE.md 红线1 说"禁止新增 dependency（crate）"，但允许"现有 crate 的 feature"且须在此标注。本方案只追加 tokio features=['io-std','time']，不引入任何新 crate。

无新 unsafe；无 unwrap()/expect() 在生产路径（placeholder_handshake_meta 中有 unwrap_or_else + unreachable!() 已存在，新代码不引入）；公开 API 均补 doc comment + doctest。

connect_remote 占位返回 Err(NotImplemented) 而非 todo!() 或 unimplemented!()，符合"非测试代码不用 panic 宏"原则。

## crossModuleDeps

- zhive-cli/engine_host.rs:stop() 调 client.shutdown() 需改为 .await——stop() 已是 async fn，直接改无破坏
- zhive-tui/rpc.rs:cancel_turn 保持不变（签名不变）；cancel_session 是新增，TUI 侧无需立即使用，但可从 Client 实例调用
- zhive-core/server/handlers.rs：cancel_session 发出的 session/cancel notification 在 server 侧走 dispatch_message → router.dispatch_notification，router 在 register_engine_handlers 注册了 session/cancel notification handler（B7，调 engine.cancel_turn(thread_id)），cancel_session 即在 server 端生效
- zhive-proto/domain.rs：cancel_session 接受 &ThreadId（domain.rs:75），与 ACP CancelNotification.session_id 的语义映射（zhive SessionId ≈ ThreadId，决策已在 B1 §2.1 + A1 §6 中确认），无新类型
- connect.rs 中 perform_handshake_with_params 需要 zhive_proto::initialize::{Capabilities, Implementation, ProtocolVersion} 三类型——均已在 connect.rs:12 import

## tests

- ClientBuilder doctest：ClientBuilder::new().client_info(Implementation { name: 'test'.into(), version: '0.1'.into(), title: None }).connect_remote('ws://localhost:9000'.into()) 应返回 Err(ClientError::NotImplemented { phase: 3, .. })
- connect_remote_returns_not_implemented：#[tokio::test] 构造 ClientBuilder::default()，调 .connect_remote('ws://localhost'.into()).await，assert matches!(Err(ClientError::NotImplemented { phase: 3, .. }))
- shutdown_waits_for_reader：#[tokio::test] 用 duplex 构建 Client，reader task 关闭后 call shutdown().await 应在 <100ms 内返回（不超时）
- shutdown_timeout_abort：模拟 reader task hung（不退出），调 shutdown().await 后应在 ~5s 内返回（不阻塞超过 5s + epsilon），用 tokio::time::pause() + advance() 驱动
- cancel_session_sends_notification：#[tokio::test] stub server 等待 session/cancel notification，客户端调 client.cancel_session(&thread_id).await，验证 server 收到 {method:'session/cancel', params:{threadId:'...'}}
- cancel_session_disconnected_returns_err：连接断开后调 cancel_session，应返回 Err(ClientError::Disconnected(_) | ClientError::Io(_))
- builder_custom_client_info_in_handshake：#[tokio::test] stub server 解码 initialize request params，验证 clientInfo.name 等于 builder 设置的名称而非硬编码 'zhive-client-native'
- existing_connect_uds_backward_compat：验证 Client::connect_uds 委托 ClientBuilder::default() 后行为与之前一致（handshake stub 测试，connect.rs 已有 handshake_tests 模块，追加一条 builder_default_same_as_direct_connect）

## risks
1. shutdown 从同步改为 async——engine_host.rs:131 的 self.client.clone().shutdown() 是仅此一处的同步调用，改 .await 后如果 stop() 被非 async 上下文调用会编译失败，但 stop() 签名已是 pub async fn stop(mut self) -> ()（engine_host.rs:128），故无问题。需全局 grep 确认无其他同步调用点（当前 grep 显示仅 engine_host.rs 一处）。

2. worker_done Notify：reader task 通过 Arc<Notify> 通知，但如果 client 在 reader 还未 spawn 完毕时就调 shutdown，notify_one 可能在 notified() 等待前就发出（Notify 是 edge-trigger，不是 level-trigger），导致 shutdown 在 5s 后 timeout。对策：构造时调用 Arc::new(Notify::new())，shutdown 实现中用 let notified = self.inner.worker_done.notified(); cancel token; ...; timeout(5s, notified).await。注意 notified() future 必须在 cancel 之前创建（保证注册在 notify_one 之前）——这是 tokio Notify 的使用约定，需要代码注释说明。

3. cancel_session 与 cancel_turn 共存的 API 混淆：需要 doc comment 明确说明区别——cancel_turn 是 engine-private RPC，cancel_session 是 ACP 协议 notification。server 端在 register_engine_handlers 注册的 session/cancel handler（B7）将其映射为 engine.cancel_turn，doc comment 须说明这一服务端语义。

## recommendation
实现顺序：
1. error.rs：先加 NotImplemented 变体（最小改动，无破坏）
2. Cargo.toml：加 tokio time feature
3. transport.rs：加 worker_done Notify 字段 + notify_one 调用
4. lib.rs：更新 Inner/Client/from_split_with_meta/shutdown(async)/cancel_session
5. connect.rs：实现 ClientBuilder + perform_handshake_with_params + connect_remote 占位 + 向后兼容委托
6. engine_host.rs：改 .shutdown() 为 .shutdown().await

范围建议：
- shutdown async 改动是 P0（engine_host.rs 已 async stop，改完即可）
- ClientBuilder 是 P1（现有调用点不破坏，可安全新增）
- connect_remote 占位是 P1（保留 API 面，内部直接 Err）
- cancel_session 是 P1（新增 helper，不破坏现有 cancel_turn）
- Server 侧 session/cancel handler 不在本 Block C 范围内（client 侧补 cancel_session，server 侧 handler 由 B7 在 register_engine_handlers 注册）

所有改动约 150-200 行实际代码，无架构变动，可在单次 PR 完成。
