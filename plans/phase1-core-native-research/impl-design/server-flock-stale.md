# Server 启动锁 flock + stale socket 探活

## harnessRef
codex-rs/app-server-transport/src/transport/unix_socket.rs:93-132（prepare_control_socket_path：connect 探活→ConnectionRefused=stale→remove），134-156（acquire_app_server_startup_lock：spawn_blocking + OpenOptions + file.lock()），174-190（ControlSocketFileGuard Drop unlink）。codex-rs/app-server-transport/src/transport/mod.rs:46-48（APP_SERVER_CONTROL_SOCKET_DIR_NAME / FILE_NAME / STARTUP_LOCK_FILE_NAME 三常量）。codex-rs/app-server-transport/src/transport/unix_socket_tests.rs:144-164（app_server_startup_lock_serializes_waiters：验证第二个 acquire 阻塞直到 drop 第一个）。

## approach
**选定方案：std::fs::File::lock() + rustix::fs 不引入**。

codex 用 `file.lock()`（对应 `std::fs::File::lock`，Rust 1.89 稳定，工作区 `rust-version = "1.89"`），zhive 直接沿用——不需要 `rustix::fs::flock`，不需要加 rustix "fs" feature，零 redline impact。替代方案（rustix::fs::flock）需要在工作区 rustix features 加 "fs"，属于「加现有 crate feature」，虽不触新增 dep 红线，但 std 已满足需求故否决。

实现分为两个相互正交的函数，放在 `crates/zhive-core/src/server.rs`（内部），通过新增的 `path::startup_lock_path()` 辅助推锁文件路径：

**(a) `prepare_uds_path(path: &Path) -> std::io::Result<()>`（内部函数，替换当前 server.rs:276-291 的 remove_file 逻辑）**

```
步骤 1：父目录 chmod 0700（dir_permissions_0700 inline，与 codex prepare_private_socket_directory 一致）
步骤 2：match UnixStream::connect(path).await
  Ok(_) => return Err(io::Error::new(AddrInUse, "zhive server already running at …"))
  Err(e) if NotFound => return Ok(())          // 干净，无文件
  Err(e) if ConnectionRefused => {}            // 文件存在但无进程监听 = stale，fall through
  Err(e) => { if !path.try_exists()? { return Ok(()); } return Err(e); }
步骤 3：验证是 socket 类型（tokio::fs::symlink_metadata + is_socket()）
  若不是 socket → Err(AlreadyExists, "path exists but is not a socket")
步骤 4：tokio::fs::remove_file(path)
```

**(b) `ServerStartupLock` struct + `acquire_startup_lock(path: PathBuf) -> io::Result<ServerStartupLock>`（内部类型，`#[cfg(unix)]`）**

```rust
pub(crate) struct ServerStartupLock { _file: std::fs::File }

pub(crate) async fn acquire_startup_lock(path: PathBuf) -> io::Result<ServerStartupLock> {
    // 父目录与 socket 同目录，prepare_uds_path 已保证存在
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true).truncate(false).read(true).write(true)
            .open(&path)?;
        file.lock()?;           // 独占 flock，阻塞直到第一个进程释放
        Ok(ServerStartupLock { _file: file })
    })
    .await
    .map_err(io::Error::other)?
}
```

lock 文件路径：`path::startup_lock_path()` 返回与 socket 同目录的 `zhive-startup.lock`（eg. `$XDG_RUNTIME_DIR/zhive-startup.lock`）。

**(c) 时序整合到 `serve_uds_inner`（server.rs:260-370）**

现有流程：`remove_file → bind → set_permissions → accept loop`

替换后（`#[cfg(unix)]`）：
```
1. let _lock = acquire_startup_lock(startup_lock_path).await?;
   // 从此刻起本进程持锁，第二个进程在同一行阻塞等待而非竞争 bind
2. prepare_uds_path(socket_path).await?;
   // 连得上 → AddrInUse error，立即返回明确错误；连不上 = stale → remove
3. let listener = UnixListener::bind(socket_path)?;
4. set_permissions_0600(socket_path) （现有逻辑保留）
5. let _guard = UdsFileGuard::new(socket_path);  // Drop 时 unlink
6. drop(_lock);  // 可选：bind 完成后即可释放锁；或保留到 serve 结束都 OK
   // 建议：bind 成功后立即 drop（与 codex 不同：codex 终生持锁）
   // 理由：持锁目的是防止两进程同时到达 bind，bind 一旦成功 socket 已占用，不再需要锁序列化
7. accept loop（现有逻辑不变）
```

UdsFileGuard（替换当前的裸 remove_file 逻辑）：
```rust
#[cfg(unix)]
struct UdsFileGuard(PathBuf);
#[cfg(unix)]
impl Drop for UdsFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %self.0.display(), %e, "failed to remove uds socket on shutdown");
            }
        }
    }
}
```

**(d) ServerError 新增 variant**

```rust
/// Another zhive server is already listening on the requested UDS socket path.
#[error("zhive server already running at {path}")]
UdsAlreadyRunning { path: String },
```

（将 `prepare_uds_path` 返回的 `io::ErrorKind::AddrInUse` 在 `serve_uds_inner` 入口处转为此 variant）

**被否决方案**：直接用 `rustix::fs::flock` — 功能等效但需要在 Cargo.toml 加 rustix "fs" feature；std 1.89 已有等效 API，在工作区 `rust-version = "1.89"` 下无需绕行。

## files

- `crates/zhive-core/src/server.rs` — （1）ServerError 新增 `UdsAlreadyRunning { path: String }` variant（server.rs:128-140）；（2）新增 #[cfg(unix)] 内部函数 prepare_uds_path(path: &Path) -> io::Result<()>，替换 server.rs:276-291 的裸 remove_file；（3）新增 #[cfg(unix)] struct UdsFileGuard(PathBuf) + Drop impl；（4）新增 #[cfg(unix)] struct ServerStartupLock { _file: std::fs::File } + acquire_startup_lock(path: PathBuf) -> io::Result<ServerStartupLock>；（5）serve_uds_inner 流程改为：acquire_startup_lock → prepare_uds_path → bind → chmod → UdsFileGuard → accept loop
- `crates/zhive-core/src/server/path.rs` — 新增 pub fn startup_lock_path() -> PathBuf：从 default_socket_path() 的父目录推出 `zhive-startup.lock`；带 doc comment + doctest（assert ends_with('.lock')）

## newTypes

- pub(crate) struct ServerStartupLock { _file: std::fs::File }  // #[cfg(unix)]
- pub(crate) async fn acquire_startup_lock(path: PathBuf) -> std::io::Result<ServerStartupLock>  // spawn_blocking + OpenOptions + file.lock()
- struct UdsFileGuard(PathBuf);  // #[cfg(unix)], Drop unlinks socket file
- async fn prepare_uds_path(path: &Path) -> std::io::Result<()>  // #[cfg(unix)], connect探活+stale remove
- pub fn startup_lock_path() -> PathBuf  // path.rs, 与 default_socket_path() 同目录 zhive-startup.lock
- ServerError::UdsAlreadyRunning { path: String }  // 已有活跃 server 时的明确 error

## redlineImpact
**rustix "fs" feature：不需要加**。std::fs::File::lock() 在 Rust 1.89 稳定（工作区 `rust-version = "1.89"`），直接可用，无需 rustix::fs::flock。工作区 rustix features 保持 ["process", "std"] 不变。

**unsafe：无**。std::fs::File::lock() 是 safe API。spawn_blocking 内无 unsafe。

**unwrap/expect：不出现**。acquire_startup_lock 用 `?`；map_err 转 io::Error::other；prepare_uds_path 全程 `?`。

**新增 dependency：无**。std + tokio（已有 features）+ rustix（已有）。

**唯一的轻微注意点**：path.rs 的 `startup_lock_path()` 返回 `PathBuf`，panic-free 推导（与 socket path 同父目录）；当 default_socket_path() 返回的路径理论上无父目录时回退到 `"."`，文档说明依赖 `default_socket_path()`。

## crossModuleDeps

- crates/zhive-core/src/server/path.rs：新增 startup_lock_path()，server.rs 内 serve_uds_inner 调用；path.rs 已被 path::default_socket_path 外部暴露
- crates/zhive-cli/src/run.rs 或 boot.rs：如果 CLI 调用 serve_uds，需感知新的 ServerError::UdsAlreadyRunning 并给用户友好输出（不是本次 task 范围，但 redline 需提前暴露 variant）
- ServerError#[non_exhaustive] 标注已存在（server.rs:127），新增 variant 不破坏下游 exhaustive match

## tests

- #[cfg(unix)] #[tokio::test] stale_socket_is_replaced：在 tempdir 创建一个已停止监听的 UnixListener（bind 后 drop listener，socket 文件残留），再调 serve_uds，断言能正常启动而不报错
- #[cfg(unix)] #[tokio::test] active_server_returns_already_running：先启动一个 serve_uds，再在同路径启动第二个，断言第二个立即返回 ServerError::UdsAlreadyRunning
- #[cfg(unix)] #[tokio::test] startup_lock_serializes_concurrent_starts：spawn 两个 acquire_startup_lock 任务，断言第二个在第一个 drop 前阻塞（参考 codex unix_socket_tests.rs:144-164）
- #[cfg(unix)] #[test] uds_file_guard_removes_on_drop：创建临时 socket 文件，构造 UdsFileGuard，drop 后断言文件不存在
- #[cfg(unix)] #[tokio::test] prepare_uds_path_non_socket_file_is_rejected：在 tempdir 放一个普通文件（非 socket），调 prepare_uds_path 断言返回 AlreadyExists 错误
- path.rs doctest：assert!(startup_lock_path().to_string_lossy().ends_with(".lock"))

## risks
**flock 阻塞 tokio 线程**：acquire_startup_lock 用 spawn_blocking，不阻塞 tokio 运行时。同 codex 做法。

**prepare_uds_path 的 connect 探活 race**：从 connect 返回 ConnectionRefused 到 remove_file 之间，另一个进程可能恰好 bind，此时 remove_file 会删掉活跃 socket。缓解：启动锁确保两个进程串行执行 prepare_uds_path，race window 消失。必须在 acquire_startup_lock 之后才调 prepare_uds_path。

**std::fs::File::lock() 在 macOS 语义**：macOS flock 是 per-fd，进程内两次 open+lock 同一文件都能成功（不互斥）。zhive 用场景是多进程防重，每进程各有自己 fd，跨进程互斥正常工作。若未来同进程内调两次（不会有，RAII 阻止），注意该平台行为。

**parent dir 0700 并发创建**：tokio::fs::create_dir_all 是幂等的；若两个进程同时创建，第二个拿到 AlreadyExists 后继续即可（标准行为）。

**UdsFileGuard Drop 在 async 上下文**：Drop 内调 std::fs::remove_file（同步 blocking），在 tokio task 的 Drop 路径上微量阻塞，可接受（与 codex 相同选择）。

## recommendation
**实现顺序**：
1. `path.rs`：加 `startup_lock_path() -> PathBuf`（3 行 + doctest，5 分钟）
2. `server.rs`：加 `ServerStartupLock` + `acquire_startup_lock`（～20 行，均 #[cfg(unix)]）
3. `server.rs`：加 `UdsFileGuard` struct + Drop（～12 行，#[cfg(unix)]）
4. `server.rs`：加 `prepare_uds_path`（～30 行，#[cfg(unix)]），替换 server.rs:276-291
5. `server.rs`：改 `serve_uds_inner` 开头，调用 acquire+prepare+guard（3 行替换旧 remove_file 块）
6. `server.rs`：ServerError 加 UdsAlreadyRunning variant + 转换点
7. 补测试（参考 tests 字段 6 项）

**范围建议**：全部在 zhive-core 内完成，不跨 crate。改动约 80-100 行（新增）+ 15 行（替换），单个 PR 范围合理。改动前先跑 `cargo check -p zhive-core --lib`，提交前 `cargo fmt --check && cargo clippy -- -D warnings && cargo nextest run -p zhive-core`。
