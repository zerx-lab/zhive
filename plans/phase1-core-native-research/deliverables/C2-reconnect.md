---
task: C2
title: 连接管理 / 重连策略（zhive-client-native lifecycle）
plan: phase1-core-native-research
date: 2026-05-28
status: draft
crate: zhive-client-native（仅依赖 zhive-proto）
depends_on:
  - deliverables/C1-client-api.md           (ClientError::Disconnected / shutdown 5s / ClientEvent 四 case)
  - deliverables/A2-initialize-capabilities.md (initialize 握手 + ProtocolVersion 协商)
references:
  - ${CODEX}/app-server-client/src/lib.rs                 (SHUTDOWN_TIMEOUT / AppServerEvent::Disconnected 一态)
  - ${CODEX}/app-server-client/src/remote.rs              (worker 写失败 / Eof / Close / Err 五条路径全部走 Disconnected + pending_requests 清理)
  - ${LSP}/src/service.rs                                  (ExitedError；exit notification 后 service 永久拒绝请求)
  - ${LSP}/src/transport.rs                                (server 退出后 server_tasks_tx.disconnect / client_abort.abort)
non-goals:
  - 不写 zhive crate 源码
  - 不改 research/99-decisions/
  - 不引 core 类型
---

> 范围声明：C2 调研产出。所有结论均锚定 codex / tower-lsp 现有行为，本 deliverable 内代码块全为草图（`todo!()` / 伪码）。
> ${CODEX} = `~/Desktop/code/github/codex/codex-rs/`；${LSP} = `~/Desktop/code/github/tower-lsp/`。
> **重要前提**：codex `app-server-client` **完全没有自动重连**，`reconnect`/`retry`/`backoff` 在 `app-server-client/src/` 全文 grep 0 命中（仅 lib.rs:278 一处文档 comment 提及 caller 自己决定 retry）。tower-lsp 同理：server 死后 `LspService` 永远返回 `ExitedError`（service.rs:27-37），caller 自己重启。**Phase 1 zhive 直接采纳「不做自动重连」**——见 §4。

---

## 1. 参考点清单

| 论断 | 仓库 / 路径 | 行号 |
|---|---|---|
| codex Disconnected 是 `AppServerEvent` 终态一案 | `${CODEX}/app-server-client/src/lib.rs` | 136 |
| codex `SHUTDOWN_TIMEOUT = 5s` 兜底 abort worker | `${CODEX}/app-server-client/src/lib.rs` | 122, 780-795 |
| codex `INITIALIZE_TIMEOUT = 10s`（initialize 阶段超时） | `${CODEX}/app-server-client/src/remote.rs` | 66, 202 |
| codex remote worker 写失败 → 移除 pending + emit Disconnected + `break`（不再 restart） | `${CODEX}/app-server-client/src/remote.rs` | 237-252 |
| codex remote `Message::Close` 收到 → emit Disconnected + `break` | `${CODEX}/app-server-client/src/remote.rs` | 403-423 |
| codex remote `Some(Err(_))` transport 错 → emit Disconnected + `break` | `${CODEX}/app-server-client/src/remote.rs` | 428-440 |
| codex remote `None` (stream 结束) → emit Disconnected + `break` | `${CODEX}/app-server-client/src/remote.rs` | 441-453 |
| codex remote 退出收尾：**遍历所有 pending_requests，一律 `Err(BrokenPipe)`** | `${CODEX}/app-server-client/src/remote.rs` | 459-467 |
| codex remote 测试 `remote_disconnect_surfaces_as_event` 验证 Disconnected 仅作为 event 抛出（无重连） | `${CODEX}/app-server-client/src/lib.rs` | 2037-2051 |
| codex `TypedRequestError` 文档明文「caller decide whether to retry」 | `${CODEX}/app-server-client/src/lib.rs` | 278 |
| codex 全文 `reconnect\|retry\|backoff` grep（app-server-client/） | `${CODEX}/app-server-client/` | 1 命中（仅文档 comment，lib.rs:278） |
| tower-lsp `ExitedError("language server has exited")` —— exit 后服务永久不可用 | `${LSP}/src/service.rs` | 27-37 |
| tower-lsp transport 读循环结束后 `server_tasks_tx.disconnect() / client_abort.abort()` —— 不重启 | `${LSP}/src/transport.rs` | 158-160 |
| tower-lsp `refuses_requests_after_shutdown` 测试：shutdown→exit 后再 call 永远返回 `ExitedError` | `${LSP}/src/service.rs` | 307-335 |
| C1 `ClientError::Disconnected(String)` —— 断连后所有 `request*` 立即返回此错 | `deliverables/C1-client-api.md` §2.2 / §3.5 | — |
| C1 `ClientEvent::Disconnected { message }` —— 事件流终态 | `deliverables/C1-client-api.md` §2.2 / §4 | — |
| C1 `Client::shutdown()` 带 5s SHUTDOWN_TIMEOUT 兜底 abort | `deliverables/C1-client-api.md` §2.2, §3.1 | — |
| A2 initialize 是强协商（ProtocolVersion + capabilities 必须重新拿） | `deliverables/A2-initialize-capabilities.md` §2-§3 | — |

---

## 2. 连接 lifecycle 状态机

> 单一公开 `Client` 内部 worker task 的状态。Phase 1 不做自动重连 ⇒ 一旦进入 `Closed`/`Disconnected` 即为**终态**；caller 想恢复 ⇒ 走 `Client::builder()...connect_*()` 从头建一个新 `Client`。

```
                       ┌──────────────────────────────────────────┐
                       │           Phase 1 lifecycle              │
                       │  (无 Reconnecting 态；caller 自己重建)     │
                       └──────────────────────────────────────────┘

   builder().connect_*()                                                      
        │                                                                     
        ▼                                                                     
   ┌──────────────┐                                                           
   │ Connecting   │  ◀── transport 层 dial (spawn child / unix connect / ws)  
   │              │      失败 ⇒ Err(ClientError::Transport)，直接 caller 拿到 
   └──────┬───────┘      （这里没有 transient Disconnected 中转）              
          │ dial 成功                                                          
          ▼                                                                   
   ┌──────────────┐                                                           
   │ Initializing │  ◀── 发 `initialize` request，等 InitializeResponse        
   │              │      10s timeout (A2 §6.Q1 / C1.initialize_timeout 默认值)
   │              │      失败 ⇒ Err(ClientError::Transport / Server)，         
   │              │              transport 立即 drop（worker 不进 Ready）       
   └──────┬───────┘                                                           
          │ initialize 成功 + 发 `initialized` notification                    
          ▼                                                                   
   ┌──────────────┐                                                           
   │   Ready      │  ◀── tokio::select! 三路：command_rx / transport.recv /   
   │              │      shutdown_signal。pending_requests HashMap 活跃中。   
   └──────┬───────┘                                                           
          │ 任一终态触发器：                                                    
          │  (a) transport.recv = Eof / Close / Err                            
          │  (b) transport.send 写失败 (server 进程死了 / UDS 文件消失)         
          │  (c) caller drop(Client) ⇒ command_tx closed                       
          │  (d) caller 显式 client.shutdown()                                  
          ▼                                                                   
   ┌──────────────┐                                                           
   │ Closing      │  ◀── 1) 遍历 pending_requests，全 resolve(Err(Disconnected)
   │              │      2) 发 ClientEvent::Disconnected { message }          
   │              │      3) Drop transport（kill child / close stream）        
   │              │      此态最长持续 SHUTDOWN_TIMEOUT = 5s（C1）              
   └──────┬───────┘                                                           
          ▼                                                                   
   ┌──────────────┐                                                           
   │   Closed     │  ◀── 终态：worker task 已退出。                            
   │ (terminal)   │      此后任何 `request*` ⇒ ClientError::Disconnected     
   │              │      `next_event()` 返回 None                              
   └──────────────┘                                                           

   ┌───────────────────────────────────────────────────────────────────────┐  
   │  恢复路径（Phase 1 = caller 自己驱动）：                                │  
   │  Closed ─── drop(old Client) ─── ClientBuilder::new()...connect_*() ──┼─▶
   │            ┌─ 新 Client ─┐                                            │  Connecting
   │            │ 全新 worker  │                                            │  (上面循环)
   │            │ 全新 pending│                                            │  
   │            │ 新一轮 initialize 协商 (A2)                                │  
   │            └──────────────┘                                            │  
   └───────────────────────────────────────────────────────────────────────┘  
```

**状态转移注释**：
1. `Connecting → Initializing` 转移在 `connect_*().await` 内部完成，caller 看不到 `Connecting` 暴露态。
2. `Initializing` 失败**不**走 `Closing`，直接把 transport drop 在 `connect_*` future 内并返回 `Err`——caller 拿到 Err 时没有 `Client` 实例可访问，自然没有 pending request 需要清理。
3. `Closing → Closed` 必经的清理顺序固定为：pending → event → transport，**对齐 codex remote.rs:241-249 / 459-467 顺序**（先 pending 后 event，避免 caller 在 Disconnected event 上还能 race 一个 in-flight request 取到 stale Ok）。

---

## 3. 在线 / 离线时 pending request 处理策略表

> 列：当前 worker 状态 ↔ caller `request_typed`/`notify`/`shutdown` 的行为 ↔ 来源锚点

| 状态 | `request_typed(...).await` | `notify(...).await` | `next_event().await` | `shutdown().await` |
|---|---|---|---|---|
| `Connecting` | **不可达**（caller 还没拿到 `Client`） | 同左 | 同左 | 同左 |
| `Initializing` | **不可达**（caller 还在 `connect_*().await`） | 同左 | 同左 | 同左 |
| `Ready` | 走完整 wire：command_tx → worker → transport.send → 收响应 → oneshot resolve | 同 request 但无 oneshot 回填 | 阻塞等下一条 server→client 消息 | 进入 `Closing`，pending 全 Err |
| `Closing`（worker 退出途中） | 已 enqueue：oneshot 由清理逻辑 resolve `Err(Disconnected)`；尚未 enqueue：`command_tx.send` 失败 → `Err(Disconnected)` | 同左 | 单次返回 `ClientEvent::Disconnected`，下次 `None` | 二次调用 ⇒ `Err(Disconnected)` 或被 owner 借用约束阻止 |
| `Closed` | 立即 `Err(ClientError::Disconnected(msg))`（command_tx 已关） | 同左 | `None`（终态） | 同 Closing；通常 Drop 已隐式完成 |

**锚点**：
- codex remote.rs:241-243（pending 在写失败时立即 remove + Err）
- codex remote.rs:459-467（worker 退出收尾，**所有** pending 一律 `BrokenPipe`）
- codex lib.rs:780-795（`shutdown` 路径：先 oneshot 等 close、再 5s timeout abort worker handle）
- C1 §3.5（zhive `Disconnected(String)` case 同时投递到 event stream + 每个 in-flight oneshot）

**关键 invariant**（与 C1 对齐）：
1. `Ready → Closing → Closed` 一旦触发，**worker 不再接受新 command**（command_tx 是 mpsc，worker 一旦 break 出 select 循环，sender 端 send 立即 Err）。
2. **不存在「等 reconnect」语义**：caller 在 `Disconnected` 事件后 `request_typed` 不会被悬挂，直接 `Err(Disconnected)`——避免 codex 同款 `request_handle.clone()` 多 task 用例在断连时静默 hang。
3. **lossless notification 与 pending request 在 Closing 期的顺序**：先把 pending 全 reject 再 emit Disconnected event。caller 用 `tokio::select!` 同时 await `request + next_event` 时，先看到 request 端的 Err，再看到 event 端的 Disconnected——语义一致（先错后讣告）。

---

## 4. Phase 1 自动重连决策

### 决策：**不做自动重连，也不提供 `client.reconnect()` 方法。Disconnected = 终态。caller 想恢复 = 走 `Client::builder()` 从头建一个新 `Client`。**

### 选 A 不选 B 的理由

**选项 A（采用，本决策）**：Disconnected 终态 + caller 自己重建
**选项 B**：内置 backoff + 自动 `reconnect()`（exponential backoff，HashMap 内 retry pending request）
**选项 C**：提供 `client.reconnect()` 显式 method，自动不做、显式可用

**为什么 A**：

1. **codex 是 A**（铁证）：`app-server-client/src/` 全文 grep `reconnect|retry|backoff` 仅命中 1 次（lib.rs:278 文档 comment「callers can decide whether to retry」），实际逻辑零。Disconnected 是 `AppServerEvent` 终态一案，worker `break` 退出后没有 restart 路径。codex 是同需求最成熟的实现，调研直接采纳。
2. **tower-lsp 是 A**（侧证）：`ExitedError("language server has exited")` 后 service 永久拒绝（service.rs:27-37, 307-335）。LSP spec 也不要求 client 自动重启 server。
3. **Phase 1 范围爆炸风险**：自动重连要解决至少 6 个子问题——
   - backoff 策略（exp / linear / fixed / jittered）—— 引 `tokio-retry` 或自己实现，新依赖（CLAUDE.md 禁止）。
   - in-flight request 是 retry 还是 fail（与 §3 表强耦合）。
   - reverse-RPC pending response 跨重连怎么办（server 重启后 id 空间重置，旧 id 无主）。
   - initialize 重协商（A2 §6.Q1 ProtocolVersionUnsupported 后是否降版本重试）。
   - UDS path 变更 / child PID 变更检测（§6）。
   - Drop / shutdown / reconnect 三态互锁。
   Phase 1 目标是「跑通 stdio + uds」（D-004），自动重连属于 Phase 2/3 的 robustness layer。
4. **caller 侧重建成本低**：`Client::builder().client_info(...).capabilities(...).connect_stdio(child).await?` 三行复用——caller 比 client 更知道是否要重启（例如 TUI 可能想提示用户、bridge 可能想直接 fail）。这是 Unix 哲学：library 不替 caller 决策。
5. **A2 协商语义干净**：每次新 Client 一定走完整 `initialize`，不存在「上次 capabilities 还能不能信」的歧义——见 §5。

**不选 B 的理由**：
- 自动 backoff 在没有 supervisor abstraction 的 library 里**几乎一定写错**（典型坑：重连风暴打死刚恢复的 server）。
- 内部 retry pending request 与 idempotency 强相关，wire schema 没声明（zhive-proto 当前无 `Idempotency-Key` header），盲 retry 会导致 thread/turn 重复创建。
- Phase 1 没有 server-side 持久化（D-002 锁定 Phase 1 仅 in-memory store），重连后 server 不认识旧 thread_id，retry 直接 -32602 `InvalidParams`。

**不选 C 的理由**：
- `client.reconnect()` 表面上「无害的可选 API」，但实际要回答 §3 表的 Closed 列「reconnect 后旧 pending 怎么办」。一旦回答了就等于做了一半 B，**沉没成本陷阱**。
- caller 侧 `drop + builder` 模式更显式：reconnect 是 caller 的 policy 决策，不是 library 的暗箱行为。
- Phase 2/3 真要做也是先做 B（自动）；C 是中间态产物，跳过即可。

**TODO** 留给 Phase 2 的设计：见 §8。

---

## 5. 重连后 `initialize` 重协商策略

> 「重连」在 Phase 1 = caller drop 旧 Client + 建新 Client。本节回答：新 Client 是否必须重新跑 `initialize`？

### 决策：**新 Client 必须重新跑完整 `initialize` 握手，零复用上次的 capabilities / protocol_version。**

### 理由（带锚点）

1. **A2 §2 强制握手**：`initialize` 是 zhive 协议 wire 层 invariant——D-007 锁定「强制 initialize 握手」。client 不发 initialize 就调任何 method ⇒ server 应当 -32002 (假定常量，A2 §6 落 wire 表后定值) 拒绝。
2. **codex 也是这样**：codex `RemoteAppServerClient::connect` 内部 `initialize_remote_connection` 是 `connect` 的强制一步（remote.rs:163-182, 798-933），caller 没有跳过握手的入口。zhive `ClientBuilder::connect_*()` 同语义。
3. **server 可能升级了**：caller 重建 Client 的常见场景是 server 升级 / 重启。如果复用旧 capabilities，caller 可能调用一个**新版本不再支持**的 method 拿 -32601。
4. **ProtocolVersion 必须重协商**：A2 §3 协商规则 `min(server_latest, request)`。server 升级后可能支持更新版本，复用旧版会卡在 V1 而错过 V2 新功能；server 降级（少见）后旧版可能不支持，复用会发出无效 wire 调用。
5. **复用 caller-side builder 配置**：caller 在自己代码里持有 `Implementation` / `Capabilities` 模板 struct 即可，每次 `ClientBuilder::new().client_info(self.info.clone()).capabilities(self.caps.clone()).connect_*(...)`——零额外 client API 表面，零 staleness 风险。

### 与 C1 / A2 接口

- C1 草图 `connect_*` 已经内嵌 `initialize` 调用（C1 §2.2 builder 终态方法描述）——本决策不增加 API 表面，只锁定**不要给 builder 加 `skip_initialize: bool` 之类 shortcut**。

**未决项 → §8**。

---

## 6. UDS path 变更检测

### 决策：**Phase 1 不监听 UDS path；caller 调 `Client::builder().connect_uds(path)` 时按当下路径 connect，之后 path 变没变不关心。断连后 caller 自己重新解析路径再建新 Client。**

### 三种方案对比

| 方案 | 触发时机 | Phase 1 取舍 |
|---|---|---|
| **A. 仅在 `connect_uds(path)` 调用时 lookup** ✅ **本决策** | caller 主动 | 零依赖、零后台 task。已断的 UDS 文件不复存在 ⇒ 直接 `Err(ClientError::Transport)`，caller 自己重试 |
| B. worker 后台 poll path（每 N 秒 stat） | 后台 timer | 新增 timer task + 设计 poll 间隔 + 与 select! 第四 arm 冲突。Phase 1 不引 |
| C. inotify / kqueue 监听父目录 | 内核事件 | 需要 `notify` crate（新依赖，CLAUDE.md 禁），跨平台兼容（Windows 无 inotify）——Phase 1 强行做亏 |

### 与连接断开检测的关系

- UDS path 文件被删除后**已建立的连接不会立即断**（Unix socket 是 inode 引用计数，path 仅在新 connect 时 lookup）。
- 真正的断连信号来自 `transport.recv() = Eof / Err`——server 进程死了 → kernel 关闭 socket pair → client 端 read 返回 0 字节 ⇒ 走 §2 状态机 `Ready → Closing`。
- 所以「UDS path 消失」不是一个 client 需要主动检测的事件；它只在 caller 想**重连**时才有意义（path 还在不在决定了新 connect 能不能成功），而 Phase 1 caller 自己负责重连，自然自己负责重新 resolve path。

### codex 参照

- codex `RemoteAppServerEndpoint::UnixSocket { socket_path }`（remote.rs:78-80）只在 connect 时用 path，后续 worker 不持有 path 字符串——zhive 同模式。

---

## 7. 关键问题逐条作答

### Q1：Phase 1 是否做自动重连？codex 怎么做？

**Phase 1 不做。** codex 也不做：`app-server-client/src/` 全文 grep `reconnect|retry|backoff` 仅 1 命中（lib.rs:278 文档 comment 提示 caller 自决）。Disconnected 是 `AppServerEvent` 终态一案（lib.rs:136）；worker break 后没有 restart 路径，pending 全 reject `BrokenPipe`（remote.rs:459-467）。tower-lsp 同样把 server exit 当终态（service.rs:27-37 `ExitedError`）。zhive 直接采纳：`Disconnected` 终态 + caller 自己 `Client::builder()...connect_*()` 重建。**不**给 explicit `reconnect()` method（避开「reconnect 后旧 pending 怎么办」语义陷阱，详见 §4 「不选 C」）。

### Q2：重连后已发出未回的请求怎么办？

**直接 error，不 retry 不 cancel。** 与 C1 `ClientError::Disconnected` resolve 策略对齐：worker 在 `Closing` 阶段遍历 `pending_requests` HashMap，对每个 in-flight oneshot 一律 `send(Err(Disconnected))`——caller 的 `request_typed.await` 立即返回 `ClientError::Disconnected`。**对齐 codex remote.rs:459-467**。理由：(a) wire 层无 idempotency key（B 系列未引入），retry 会重复创建 thread/turn；(b) server 重启后旧 thread_id 不存在，retry 必 -32602；(c) caller 比 library 更知道哪些请求该重发（例如读操作可重，写操作不应重）。

### Q3：UDS 路径 server 重启后变了怎么发现？

**不主动发现。** caller 在 `Client::builder().connect_uds(path)` 调用时按当下 path lookup；之后 path 不再 tracked。断连信号走 `transport.recv() = Eof/Err`（不依赖 path 状态）。caller 想重连 ⇒ caller 自己 resolve 新 path（例如读 `/run/zhive/agent.sock` 的 symlink 目标 / 查 service registry）→ 新 `connect_uds(new_path)`。理由：(a) inotify 跨平台烂（Windows 无）+ 需新 crate（CLAUDE.md 禁）；(b) poll path 需后台 timer task + 选择 poll 间隔；(c) Phase 1 不引这俩复杂度。codex `RemoteAppServerEndpoint::UnixSocket` 同模式：path 只在 connect 时用一次（remote.rs:78-80）。

---

## 8. 未决项

> TODO(开放项 C2-N1)：Phase 2/3 引入自动重连时的设计草案。建议形态：`ClientBuilder::reconnect_policy(ReconnectPolicy)` 三态枚举 —— `Never`（Phase 1 默认）/ `Bounded { max_attempts, backoff }` / `Unbounded { backoff }`；in-flight pending 在重连开始时全 `Err(Disconnected)`（即 B 方案不 retry pending，只重连 transport），caller 拿到 Err 自行决定是否重发。**依赖**：B 系列引入 `Idempotency-Key` header（thread/turn create 类操作幂等化）后才稳妥；否则任何 retry 都可能创建鬼影 thread。

> TODO(开放项 C2-N2)：`connect_stdio(child)` 在 child 进程退出后的语义。child 死 ⇒ stdin/stdout 被内核关闭 ⇒ `transport.recv = Eof` ⇒ 走 §2 `Ready → Closing`。但 child 的 exit_status 此时已可读，caller 想拿这个 exit_status 做 diagnostic 怎么办？建议在 `ClientEvent::Disconnected.message` 里附加 child 的 `try_wait()` 结果字符串（best-effort，不阻塞 worker）。C2 调研期不动 API surface，仅记录。

> TODO(开放项 C2-N3)：`Closing` 状态下 `shutdown().await` 与 `Drop` 的相互作用。若 caller 在 worker 已自然 break 后再调 `shutdown()`，oneshot 的另一端可能已被 drop ⇒ 应返回 `Ok(())` 而非 `Err`（语义：已经关了，无需操作）。codex `shutdown` 走 5s timeout 后强 abort（lib.rs:780-795），但没明确处理「worker 已退」case，需 zhive 实现期 review。

> TODO(开放项 C2-N4)：reverse-RPC pending response 在断连时是否需要发 `Cancelled` outcome（A3 §6.2 决策同源）？目前草图：worker 在 `Closing` 阶段已无法发送任何 wire 消息（transport 已坏），所以**不发** Cancelled，让 server 自己在收到 RST/EOF 后 timeout reverse-RPC pending。这与「正常 cancel 时优先发 Cancelled」语义有别，需在 A3/C3 落地时统一。

> TODO(开放项 C2-N5)：是否在 builder 暴露 `health_check_interval`（即使不做自动重连，也可以发 `ping` notification 做存活探测）？codex 没有；ACP 也没有标准 ping。建议 Phase 1 不做，Phase 2 与重连策略一起设计——单独的 health check 没意义，发现死了也只能 emit Disconnected（与等 transport.recv = Eof 同效果，多一个 timer 干扰）。

---

## 9. 验收对照

- [x] 论断带锚点（§1 / §3 / §4 / §7 全部 verbatim 引用 codex + tower-lsp 行号）
- [x] 不动 `crates/` 源码（本 deliverable 零 Rust 代码块；伪码仅在 §2 ASCII 状态机）
- [x] 不改 `research/99-decisions/`（§8 未决项均在本文件内）
- [x] 不 `git pull`（codex 读 2 文件 / tower-lsp 读 2 文件，每 repo ≤ 3 文件上限内）
- [x] 仅依赖 zhive-proto + C1 公开类型，不引 core
- [x] 25-40 min 内落盘
