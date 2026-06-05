# CLI/TUI 裸输出甄别改造（debug/print → 日志 vs 保留用户输出）

## currentState
crates/zhive-cli/src/run.rs 是唯一含裸输出的文件。其他 crate 全部干净（zhive-core/zhive-tui/zhive-proto/zhive-bridge-acp/zhive-bridge-stdio/zhive-client-native/zhive-mcp 均无非 doctest 的 print 语句）。

精确状态：
- run.rs:323  print!("{text}")                  ← exec headless AgentMessage 用户输出
- run.rs:325  println!("\n[tool] {name}")        ← exec headless ToolCall 用户输出
- run.rs:338  print!("{delta}")                  ← exec headless delta 流 用户输出
- run.rs:343  println!()                         ← exec turn_completed 尾换行 用户输出
- run.rs:425-430 tracing::info!(name: "server.serve.start", ...) ← serve 启动状态
- run.rs:442  tracing::info!(name: "server.shutdown", "engine shutting down") ← serve 关机状态
- run.rs:566  println!("{}", path.display())     ← config path 用户输出
- run.rs:576  println!("wrote sample config to ...") ← config init 用户输出
- run.rs:603-604 println!("config: ...")         ← doctor 用户输出
- run.rs:615-616 println!("provider: ...")       ← doctor 用户输出
- run.rs:621  println!("provider: ...")          ← doctor 用户输出
- run.rs:626  println!("mcp: ...")               ← doctor 用户输出
- run.rs:636-641 println!("skills: ...")         ← doctor 用户输出
- run.rs:646  println!("skills: ...")            ← doctor 用户输出
- run.rs:653-654 println!("data-dir: ...")       ← doctor 用户输出
- run.rs:657  println!("data-dir: ...")          ← doctor 用户输出
- run.rs:785  let _ = std::io::stdout().flush()  ← 测试代码（#[tokio::test]），无需改

TUI 已有文件日志：run.rs:76-113 init_tui_file_logging() 用 tracing_subscriber::fmt().with_writer(Arc<File>).try_init()，TUI 模式下终端不会被 tracing 污染。
非 TUI 的 serve/exec/acp 已有 init_stderr_logging()（run.rs:479-486）：tracing 写 stderr，stdout 留给 wire/用户输出。
B9 deliverable §4.1 日志级别约定：info = 生命周期里程碑；warn = 可恢复异常；error = 不可恢复。
B9 §4.3 明确禁止：❌ 用 eprintln! —— 全部走 tracing。

## harnessRef
codex-rs/app-server/src/main.rs（无 eprintln，全走 tracing::info/tracing::warn；serve 启动/关机用 tracing::info! 而非 eprintln!）。codex-rs/app-server/src/outgoing_message.rs:1047,1162,1169（RPC span 用 tracing::info_span! 含 rpc.method 字段，OTel 对齐范式）。B9 deliverable §4.2 关键示例（run.rs 改写时直接参考该代码片段的字段命名约定）。

## approach
逐行甄别表（文件:行 → 分类 → 改法）

| 行号 | 当前代码 | 分类 | 改法 |
|------|---------|------|------|
| run.rs:323 | print!("{text}") | USER_OUTPUT 保留 | 不改，headless exec 的 stdout 契约 |
| run.rs:325 | println!("\n[tool] {name}") | USER_OUTPUT 保留 | 不改，headless 工具活动提示 |
| run.rs:338 | print!("{delta}") | USER_OUTPUT 保留 | 不改，流式 token 输出 |
| run.rs:343 | println!() | USER_OUTPUT 保留 | 不改，尾换行确保脚本能解析 |
| run.rs:425-430 | eprintln!("zhive engine serving on {} · provider={} model={}", ...) | DIAGNOSTIC 改 tracing | 替换为 tracing::info!(socket = %socket.display(), provider = %cfg.active_provider_label(), model = %cfg.active_model(), "engine serving") |
| run.rs:442 | eprintln!("zhive: shutting down") | DIAGNOSTIC 改 tracing | 替换为 tracing::info!("engine shutting down") |
| run.rs:566 | println!("{}", path.display()) | USER_OUTPUT 保留 | 不改，config path 是 CLI 命令期望输出 |
| run.rs:576 | println!("wrote sample config to {}", path.display()) | USER_OUTPUT 保留 | 不改，config init 成功反馈 |
| run.rs:603-604 | println!("config: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:615-616 | println!("provider: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:621 | println!("provider: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:626 | println!("mcp: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:636-641 | println!("skills: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:646 | println!("skills: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:653-654 | println!("data-dir: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:657 | println!("data-dir: ...") | USER_OUTPUT 保留 | 不改，doctor 报告 |
| run.rs:785 | let _ = std::io::stdout().flush() | TEST 代码，保留 | #[tokio::test] 内，不改 |

设计原则：
1. headless exec stdout 契约（run.rs:323/325/338/343）：agent 回复走 stdout 是 piping 契约，绝对保留。init_stderr_logging() 已保证 tracing 只写 stderr，不干扰 stdout。
2. serve 的两条 eprintln!（run.rs:425-430, 442）：serve 模式下 stdout 可用（不是 ACP wire），但 B9 §4.3 明确禁止 eprintln! 走法，改 tracing::info!。serve 已调 init_stderr_logging()，所以 tracing 输出到 stderr 是正确的。
3. config/doctor 的 println!（run.rs:566-657）：这些是 CLI 子命令的正常用户输出（机器可解析的报告），tracing 不适合（tracing 加了 timestamp/level 前缀会破坏脚本解析）。保留 println!。
4. TUI 模式：run_tui 调 init_tui_file_logging()（run.rs:43）而不是 init_stderr_logging()；tracing 写文件，终端干净。TUI 内无裸输出，不受影响。
5. 全 crate 扫描结论：zhive-core 的 3 处 println! 全在 /// doctest 注释内（state_db.rs:335, memories_db.rs:147, engine.rs:716），不是运行时输出，不改。

被否决的备选：
- 把 serve 的两条 eprintln! 改成 println! —— 错误，println! 也是裸输出，且 B9 明确禁止。
- 把 doctor/config 改成 tracing::info! —— 错误，tracing 输出带前缀（时间戳/级别）会破坏 "config:   /path/to/file" 这类机器可解析格式，且 doctor 命令在非 serve 模式下未必启动 tracing subscriber。

## files

- `crates/zhive-cli/src/run.rs` — 将 run_serve 内 run.rs:425-430 的 eprintln! 替换为 tracing::info!(socket = %socket.display(), provider = %label, model = %model, "engine serving")；将 run.rs:442 的 eprintln! 替换为 tracing::info!("engine shutting down")。其余 println!/print! 全部保留不动。无需新增文件。

## newTypes

- // 无新增类型；仅替换 2 处 eprintln! 为 tracing::info!

## redlineImpact
无新增 crate 依赖。tracing 已在 workspace（run.rs 内其他路径已有 tracing::warn! 用法，如 run.rs:140-144, 252-256, 358）。无 unsafe。无 unwrap/expect。改动极小（2 行替换），不触任何红线。

## crossModuleDeps

- B9-tracing.md §4.1 日志级别约定（info = 生命周期里程碑）：serve 启动/关机恰好符合 info! 级别定义
- B9-tracing.md §5.2 subscriber 初始化点：run_serve/run_exec/run_acp 已各自调 init_stderr_logging()，改完后 tracing::info! 自动路由到 stderr，无需修改 main.rs
- run.rs:43 init_tui_file_logging()：TUI 路径完全隔离，此次改动不影响 TUI

## tests

- cargo check -p zhive-cli --lib 验证无编译错误
- cargo clippy -p zhive-cli -- -D warnings 验证无 eprintln! 残留（可写 clippy::restriction lint 或人工 grep 检查）
- cargo nextest run -p zhive-cli 确保现有测试（exec_args_parse_*、doctor_command_parses、doctor_output_contains_key_fields 等 run.rs:681-820）全部通过
- 手工集成验证 serve：zhive serve 启动后 stderr 可见 'engine serving' 信息行，stdout 干净；Ctrl-C 后 stderr 可见 'engine shutting down'
- 手工集成验证 exec：zhive exec -p 'hello' 的 stdout 仅含 agent 回复，tracing 事件在 stderr

## risks
改动极小（2 处 eprintln! → tracing::info!），唯一风险是用户在 shell 脚本里 parse serve 的 stderr 输出（如 grep 'serving on' 来获取 socket 路径）。改后字段格式从 "zhive engine serving on /path · provider=x model=y" 变为 tracing fmt 格式 "INFO zhive_cli::run engine serving socket=/path provider=x model=y"（字段位置和格式变化）。缓解：在 serve 的 doc comment 补一行说明格式，且 tracing fmt 的 key=value 格式仍可 awk 解析。低风险，无回滚需要。

## recommendation
实现顺序：先改 run.rs:425-430（eprintln! → tracing::info!），再改 run.rs:442，共 2 处。cargo check -p zhive-cli 验收，再跑 nextest。改动行数 4-5 行，10 分钟内完成。不需要单独 PR，可与 task-9（OTel feature gate 收口）合并一个 commit 提交，因为两者都属于「日志基础设施收口」范畴（MEMORY.md task #9）。doctor/config 的 println! 保留不动——这是正确的 CLI 用户输出模式，不是技术债。
