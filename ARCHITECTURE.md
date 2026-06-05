# zhive 架构

zhive 是一个 Rust 编写的终端 AI 协作 agent，核心是**一个引擎 + 多客户端**的 daemon 式架构。
设计的轴心：**`zhive-core` 是内核 → 凝结成 `zhive-proto` 这套统一 harness API → 基于它可以长出任意产品，TUI 只是第一个。**

> 所有架构决策的权威来源是 [`research/99-decisions/README.md`](research/99-decisions/README.md)（D-001 ~ D-015）。本文是对其落地结构的可视化导览。当前处于 **Phase 1**。

---

## 设计哲学

1. **协议即唯一边界** —— 任何客户端（连同进程内 TUI）都只能通过 `zhive-proto` 的 wire schema 访问引擎，杜绝内存捷径，确保所有产品自动同级。
2. **adapter 隔离上游风险** —— 不稳定的外部生态依赖（ConnectRPC/rmcp/ACP/a2a）被挡在 bridge crate 外，并用"单维护者+pre-1.0 即拒收/精确锁"的统一标尺管理（D-003/005/015）。
3. **该拆的一开始就拆，该收敛的坚决收敛** —— SQLite 4 库从第一天就分（避免日后 migration 学费），但 crate 数从 12 砍到 7（避免 import 折磨）。
4. **每个扩展方向都有 trait 留口但不空建 crate** —— Phase 2/3 的能力在 schema/trait 层已预留承载位，但不提前造空壳。

---

## 图 1：从核心向外扩展的分层架构

```mermaid
graph TD
    subgraph L0["🫀 内核层 · 引擎本体"]
        CORE["<b>zhive-core</b><br/>Thread/Turn/Item 状态机 · 工具分发<br/>权限 reducer · hooks · 取消传播 · 持久化<br/>+ <b>core::server</b> module（JSON-RPC 网关）"]
        PROV["Provider 边界<br/>llmsdk::DynLanguageModel"]
        STORE["持久化<br/>JSONL rollout（真相源）+ 4×SQLite"]
        CORE --- PROV
        CORE --- STORE
    end

    subgraph L1["🔌 统一 Harness API · 唯一边界"]
        PROTO["<b>zhive-proto</b><br/>JSON-RPC 2.0 wire schema（serde+schemars）<br/>+ LSP 风格 framing<br/>initialize 握手 · 反向 RPC · 事件流<br/><i>所有产品只能通过这层访问引擎</i>"]
    end

    subgraph L2["🧰 客户端 SDK 层"]
        NATIVE["<b>zhive-client-native</b><br/>Rust JSON-RPC client<br/>事件订阅 · 反向 RPC handler"]
    end

    subgraph L3["📦 产品层 · 基于 harness API 的各种形态"]
        TUI["<b>zhive-tui</b><br/>ratatui 终端 UI<br/>(D-002 不依赖 core)"]
        BRIDGE["<b>zhive-bridge-stdio</b><br/>stdio↔UDS 转发<br/>(供 ACP/MCP 宿主 spawn)"]
        WEB["Web UI<br/><i>Phase 3 · 复用同一 schema</i>"]
        IDE["IDE / 远程客户端<br/><i>未来 · 同级扩展</i>"]
        EXEC["zhive-exec headless<br/><i>Phase 2</i>"]
    end

    subgraph L4["🚀 宿主 / 入口"]
        CLI["<b>zhive-cli</b><br/>分发器 binary：进程关注点<br/>config / API key / in-process engine host"]
    end

    CORE -->|"暴露 serve_stdio / serve_uds"| PROTO
    PROTO -->|"wire 契约"| NATIVE
    NATIVE --> TUI
    PROTO -.->|"纯字节转发，不解析"| BRIDGE
    PROTO -.->|"复用 schema"| WEB
    PROTO -.->|"复用 schema"| IDE
    NATIVE -.-> EXEC

    CLI -->|"feature=engine 内嵌引擎"| CORE
    CLI -->|"组装并启动"| TUI
    CLI --> BRIDGE

    classDef core fill:#7c3aed,stroke:#4c1d95,color:#fff
    classDef api fill:#0891b2,stroke:#164e63,color:#fff
    classDef sdk fill:#0d9488,stroke:#134e4a,color:#fff
    classDef product fill:#1f2937,stroke:#374151,color:#e5e7eb
    classDef future fill:#1f2937,stroke:#4b5563,color:#9ca3af,stroke-dasharray: 5 5
    classDef host fill:#b45309,stroke:#78350f,color:#fff

    class CORE,PROV,STORE core
    class PROTO api
    class NATIVE sdk
    class TUI,BRIDGE product
    class WEB,IDE,EXEC future
    class CLI host
```

**读图要点（从内向外）：**
- **内核 `zhive-core`** 是一切的源头——引擎状态机 + 内嵌的 JSON-RPC server module。
- 引擎能力凝结成 **统一 harness API（`zhive-proto`）**，这是**唯一边界**：任何产品、连同内嵌 TUI，都只能通过这层 wire schema 访问引擎，没有内存捷径。
- 往外是 **客户端 SDK（native）** 和**各种产品形态**——TUI 只是其中之一，与 Web/IDE/远程/headless 同级。
- `zhive-cli` 是**宿主入口**而非顶层依赖：它负责进程关注点（配置、密钥、把引擎和产品组装起来），但引擎和 UI 都不依赖它。

---

## 图 2：依赖方向（谁依赖谁）

```mermaid
graph BT
    PROTO["zhive-proto<br/><i>统一 API · 零内部依赖</i>"]
    CORE["zhive-core"]
    NATIVE["zhive-client-native"]
    TUI["zhive-tui"]
    BRIDGE["zhive-bridge-stdio"]
    CLI["zhive-cli"]

    CORE --> PROTO
    NATIVE --> PROTO
    TUI --> NATIVE
    TUI --> PROTO
    BRIDGE --> PROTO
    CLI --> TUI
    CLI -->|feature=engine| CORE
    CLI --> BRIDGE
    CLI --> NATIVE

    classDef api fill:#0891b2,stroke:#164e63,color:#fff
    classDef core fill:#7c3aed,stroke:#4c1d95,color:#fff
    classDef other fill:#1f2937,stroke:#374151,color:#e5e7eb
    class PROTO api
    class CORE core
    class NATIVE,TUI,BRIDGE,CLI other
```

> 箭头 = "依赖"。注意 **`zhive-tui` 不依赖 `zhive-core`**（D-002）——它只依赖 client + proto，证明"TUI 只是 harness API 的一个消费者"。`zhive-proto` 是图的汇聚点：所有人都指向它，它谁也不依赖。

---

## 图 3：运行时——同一套 API，多种接入路径

```mermaid
flowchart LR
    subgraph clients["产品 / 客户端"]
        TUI["zhive-tui"]
        ACP["外部 ACP/MCP 宿主<br/>(Zed/Cursor/Claude Desktop)"]
        FUT["未来 Web/远程客户端"]
    end

    subgraph api["统一 Harness API（JSON-RPC 2.0 over Transport）"]
        UDS{{"UDS transport<br/>$XDG_RUNTIME_DIR/zhive.sock"}}
        STDIO{{"stdio transport"}}
        REMOTE{{"远程 transport<br/>Phase 3 · RpcTransport trait"}}
    end

    subgraph engine["zhive-core 引擎"]
        SERVER["core::server<br/>serve_loop · Router · 反向RPC"]
        ACTOR["EngineInner actor<br/>Thread/Turn/Item"]
        SERVER --> ACTOR
    end

    TUI -->|client-native| UDS
    ACP -->|spawn| BR["bridge-stdio<br/>纯字节转发"]
    BR --> UDS
    ACP -.直连.-> STDIO
    FUT -.-> REMOTE

    UDS --> SERVER
    STDIO --> SERVER
    REMOTE -.-> SERVER

    ACTOR -.事件流/反向RPC.-> SERVER

    classDef api fill:#0891b2,stroke:#164e63,color:#fff
    classDef core fill:#7c3aed,stroke:#4c1d95,color:#fff
    classDef cl fill:#1f2937,stroke:#374151,color:#e5e7eb
    class UDS,STDIO,REMOTE api
    class SERVER,ACTOR core
    class TUI,ACP,FUT,BR cl
```

**核心信息**：引擎对外只暴露一套 harness API，但 API 可以走多种 transport（UDS / stdio / 未来远程）。不同产品按自己的接入能力选择路径——TUI 走 UDS，只能 spawn 子进程的编辑器走 bridge-stdio 转发，未来 Web/远程走 `RpcTransport` 抽象的新 transport。**新增一种产品 = 在这套 API 上加一个消费者，引擎零改动。**

---

## 图 4：一次 Turn 的运行时数据流

```mermaid
sequenceDiagram
    participant C as Client
    participant S as core::server<br/>(serve_loop/Router)
    participant E as EngineInner<br/>(actor loop, 串行)
    participant T as run_turn<br/>(独立 task ≤32 循环)
    participant P as Provider<br/>(llmsdk)
    participant W as StorageWriter<br/>(独立 task)

    C->>S: engine/start_turn (JSON-RPC)
    Note over S: 握手门：首条必须 initialize
    S->>E: Submission::StartTurn (+oneshot)
    Note over E: phase Idle→Turn · drain NextTurn 注入队列<br/>分配 TurnId · 创建 ActiveTurn(cancel token)
    E-->>C: oneshot 回复 TurnId + 广播 TurnStarted
    E->>T: tokio::spawn run_turn

    loop 每轮迭代
        T->>T: build_call_options(items_tail)
        T->>P: provider.do_stream
        P-->>T: StreamPart 流
        Note over T: StreamFold 折叠 → Item
        T-->>C: 广播 ItemAppended / ItemDelta(实时)
        T->>W: StorageWriteOp (JSONL 先 fsync, SQL 异步追赶)
        alt 有 ToolCall
            Note over T: PreToolUse hook → PermissionReducer<br/>(deny>defer>ask>allow)
            opt Ask
                T-->>C: 反向RPC session/request_permission
                C-->>T: PermissionOutcome
            end
            Note over T: 执行工具 → PostToolUse hook
        else 无工具 / hook stop / 达上限
            T->>E: finish_turn (phase Turn→Idle)
            E-->>C: 广播 TurnCompleted
            opt item ≥ 阈值
                Note over E: run_compaction：LLM summary 替换历史
            end
        end
    end
```

**关键运行时设计：**
- **Actor 模型**：`EngineInner` 串行消费 `Submission`，`start_turn` 立即回 `TurnId` 后把实际 turn `spawn` 出去，保证 actor loop 始终能响应 `CancelTurn`。
- **三层原语 Thread→Turn→Item**（D-006），Item 含 reasoning/tool_call/exec/file_edit/agent_message/diff/terminal/thought。
- **反向 RPC**（D-008）：权限审批走 server-initiated request，与事件同一条 stream。四态 reducer；subagent 强制继承父 scope 且只能缩窄。
- **三条注入队列**（Pi 模型）：Steer（每次 LLM call 前）/ FollowUp（turn 边界）/ NextTurn（下轮，abort 不清空）。
- **取消传播树**：Engine→Turn→Tool/Hook/Subagent 层级 CancellationToken。
- **持久化 JSONL-first**（D-011）：rollout JSONL 是真相源（带 Leaf 指针支持 fork），4 个 SQLite 库（state/logs/memories/goals）异步追赶。

---

## Crate 一览

| Crate | 角色 | 依赖 | 备注 |
|---|---|---|---|
| `zhive-proto` | **统一 harness API**：wire schema + framing | 无内部依赖 | 汇聚点，唯一边界 |
| `zhive-core` | 引擎本体 + 内嵌 `core::server` | proto | server 非独立 crate |
| `zhive-client-native` | Rust JSON-RPC client | proto | D-002 不依赖 core |
| `zhive-tui` | ratatui 终端 UI | client-native, proto | 协议的普通消费者 |
| `zhive-bridge-stdio` | ~90 行 stdio↔UDS 纯字节转发 | proto | 禁止解析 JSON-RPC |
| `zhive-cli` | 分发器 binary（宿主入口） | tui/bridge/core(feature) | 进程关注点 |
| `xtask` | 构建/迁移工具 | 独立 | 不引 acp/rmcp |

---

## 扩展点（已留口的方向）

| 扩展维度 | 机制 | 状态 |
|---|---|---|
| 新客户端（Web/IDE/远程） | proto + client，D-002 平级 | TUI 已验证 |
| 新 transport（远程/TLS） | `Transport` / `RpcTransport` trait | stdio+UDS 已实现 |
| 新生态协议（MCP/ACP） | bridge crate + adapter trait，精确锁版本 | bridge-stdio 已实现 |
| 新 LLM provider | `llmsdk::DynLanguageModel`（不自造 trait） | Anthropic/OpenAI/Scripted |
| 新工具 | `Tool` trait + `ToolRegistry` | read/write/edit/grep/glob/agent/bash 内置 |
| 用户扩展（extension/prompt/skill） | manifest 统一发现（D-013），三层 settingSources | schema 已定 |
| Hooks | 14+ 事件，`#[non_exhaustive]`（D-012） | 已预留 5 个 Phase 2/3 事件 |
| 协议演进 | initialize + v1/v2 + 独立 capability flag（D-007） | 已实现 |
| 可观测性 | tracing spans 进核心，OTel exporter feature gate（D-014） | observability.rs |

### 三阶段路径（D-010）

- **Phase 1（当前）**：core 引擎 + proto + client-native + tui + cli + bridge-stdio + ACP minimal conformance harness。
- **Phase 2（生态接入）**：`zhive-mcp`（rmcp 1.6）、`zhive-bridge-acp`（ACP 0.13 read+write）、`zhive-exec` headless、扩展系统落地。
- **Phase 3（扩展）**：Web UI（复用 schema）、远程 TLS / 云沙箱、ConnectRPC 候选评估、A2A AgentCard schema 占位（手写 JSON，不引 a2a-rs）。
