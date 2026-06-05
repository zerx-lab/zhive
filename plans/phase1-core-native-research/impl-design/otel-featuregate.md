# OTel feature gate 收口（decision-diffs §2.6 方案 b）

设计目标：默认 `cargo check -p zhive-core --lib` 不编译 OTel 三库；otel feature 关闭时 `noop_tracer_provider()` 用 stub 路径，整个 observability.rs 不 use 任何 opentelemetry_sdk / tracing_opentelemetry 类型。zhive-cli 不引用任何 OTel crate，`run.rs` 的两个 subscriber init 路径只用 `tracing_subscriber::fmt()`，不受影响。

## harnessRef
codex-rs 未使用 feature gate 模式（codex 全量引入 OTel 无 gate），因此不借鉴 codex 架构，harness 仅作 OTel API 形态参考。zhive 自身现有 feature gate 惯用法见 `zhive-core/Cargo.toml:23-27` — `skills` 和 `tools` feature 的 optional dep 声明模式是直接参照点（`dep:serde_norway` / `dep:ignore` 等）。

## approach
选定方案：workspace 层保留三个 OTel dep 声明不变（普通声明，无 optional——`workspace.dependencies` 不能标 optional），zhive-core 侧把三者写成 `{ workspace = true, optional = true }` 并绑定到新 `otel` feature，observability.rs 用 `#[cfg(feature="otel")]` 拆分函数，无 otel 时暴露文档占位 stub。feature 默认关闭（`default = []`）。

理由：
- dep 本就存在于 workspace，crate 侧 option 化不违反"禁止新增 dependency"红线——dep 声明不变，只是默认不编译（decision-diffs §2.6 明确"feature gate 不算新增 dep"）。
- crate 侧 `optional = true` 是关闭默认编译的标准收口手段，零风险。
- stub 函数（无 otel 时返回 `()`）保持同名 `noop_tracer_provider`、doc comment 提示"otel feature 关闭时此函数为 no-op 占位"，Phase 2 在 default 特性集激活 otel 时只需删 stub 段落，无 wire 改动。

被否决的备选：
- (a) 彻底删除 workspace 三个 dep 声明 → Phase 2 需要重新 cargo add，且当前已有 git 历史记录，清理比保留更嘈杂。
- (c) 把 OTel 移到专用 zhive-otel crate → 增加 crate 边界复杂性，Phase 1 完全不需要。

## files

- `/home/zero/Desktop/code/zerx-lab/zhive/Cargo.toml` — workspace.dependencies 中三行（tracing-opentelemetry / opentelemetry / opentelemetry_sdk）保持普通声明，不加 `optional`。Cargo 不允许 `workspace.dependencies` 条目带 optional 字段——optional 必须由引用该 workspace dep 的成员 crate 自行声明。因此 workspace 侧无改动：

```toml
tracing-opentelemetry = { version = "0.33", default-features = false }
opentelemetry         = { version = "0.32", default-features = false, features = ["trace"] }
opentelemetry_sdk     = { version = "0.32", default-features = false, features = ["trace", "rt-tokio"] }
```

feature gate 完全在 crate 侧落地：zhive-core 引用时写 `{ workspace = true, optional = true }`。
- `/home/zero/Desktop/code/zerx-lab/zhive/crates/zhive-core/Cargo.toml` — 两处改动：

1. [features] 段（当前行 23-27）新增 `otel` feature：
```toml
[features]
default = []
otel    = ["dep:tracing-opentelemetry", "dep:opentelemetry", "dep:opentelemetry_sdk"]
sandbox = []
skills  = ["dep:serde_norway"]
tools   = ["dep:ignore", "dep:regex", "dep:glob"]
```

2. [dependencies] 段中行 54-56，三个 OTel dep 加 `optional = true`：
```toml
# OpenTelemetry layer for `tracing` — disabled by default (decision-diffs §2.6
# method b). Enable with `--features otel`; Phase 2 will activate this in the
# default feature set once an OTLP exporter target is confirmed.
tracing-opentelemetry = { workspace = true, optional = true }
opentelemetry         = { workspace = true, optional = true }
opentelemetry_sdk     = { workspace = true, optional = true }
```
- `/home/zero/Desktop/code/zerx-lab/zhive/crates/zhive-core/src/observability.rs` — 对 `noop_tracer_provider()` 函数（行 71-80）及其 doc comment 做 cfg 分裂，并保留 stub。三个改动点：

1. 在文件顶部模块 doc（行 1-26）中把「Phase 1 ships the tracer-provider plumbing…」段落改为：
```
//! Phase 1 gates all OTel plumbing behind the `otel` Cargo feature.
//! Without the feature, this module only exports span-name and field-name
//! constants (`spans` and `fields` submodules), which carry no dependencies.
//! Enable with `cargo check -p zhive-core --features otel` to include the
//! real `SdkTracerProvider` constructor.
```

2. `noop_tracer_provider()` 函数（行 71-80）改为 cfg 分裂的两个版本：
```rust
/// Builds a no-op tracer provider.
///
/// When the `otel` Cargo feature is **enabled**, returns a real
/// [`opentelemetry_sdk::trace::SdkTracerProvider`] with no exporter
/// attached, suitable for unit tests.
///
/// When the `otel` feature is **disabled** (default), this is a stub that
/// performs no work. Phase 2 will activate the feature and populate the
/// provider with an OTLP exporter.
///
/// # Example
///
/// ```rust
/// let _p = zhive_core::observability::noop_tracer_provider();
/// ```
#[cfg(feature = "otel")]
#[must_use]
pub fn noop_tracer_provider() -> opentelemetry_sdk::trace::SdkTracerProvider {
    opentelemetry_sdk::trace::SdkTracerProvider::builder().build()
}

/// No-op stub when the `otel` feature is disabled.
///
/// # Example
///
/// ```rust
/// zhive_core::observability::noop_tracer_provider();
/// ```
#[cfg(not(feature = "otel"))]
pub fn noop_tracer_provider() {
    // Phase 1: OTel feature gate disabled. No-op.
    // Enable `--features otel` to get the real SdkTracerProvider.
}
```

3. `#[cfg(test)] mod tests` 中的 `noop_provider_builds` 测试（行 104-106）加 `#[cfg(feature="otel")]` 守护：
```rust
#[cfg(feature = "otel")]
#[test]
fn noop_provider_builds() {
    let _p = noop_tracer_provider();
}
```
同样地，`span_emission_tests` 模块不引用 OTel 类型（使用自写 SpanCapture subscriber），该 mod 不需要 cfg guard，保持原样。

## newTypes

- pub fn noop_tracer_provider() [cfg(feature="otel")] -> opentelemetry_sdk::trace::SdkTracerProvider
- pub fn noop_tracer_provider() [cfg(not(feature="otel"))] -> ()

## redlineImpact
不触红线 1（新增 dependency）：三个 OTel crate 声明早已存在于 workspace.dependencies，改 optional=true 只是关闭默认编译开关，不是新增 crate。这一判断已在 decision-diffs §2.6 方案 b 落槌（"feature gate 不算新增 dep"，dep 已在 Cargo.toml 里）。

不触红线「unsafe」：observability.rs 全文无 unsafe，stub 也无 unsafe。

注意事项（非红线但需说明）：optional 只能由成员 crate 声明——`workspace.dependencies` 条目不能带 optional 字段（Cargo 会报错「cannot have optional workspace dependency」）。因此 workspace 侧保持普通声明，zhive-core 引用时写 `{ workspace = true, optional = true }`，由 `otel` feature 激活；rust-version 1.88 对该写法完全兼容。

## crossModuleDeps

- zhive-cli/src/run.rs（行 108、行 482）：两处 subscriber init 当前只用 tracing_subscriber::fmt()，无 OTel 引用，不需要改动。若 Phase 2 要在 CLI 中激活 OTel layer，需在 zhive-cli/Cargo.toml 中添加 zhive-core 的 otel feature 传递，并在 run.rs 的 init_tracing() 位置加 #[cfg(feature="otel")] 分支。
- zhive-core/src/lib.rs：pub use observability 的重新导出路径不受影响，noop_tracer_provider 签名变了（无 otel 时返回 ()），任何调用点需确认——grep 结果显示 noop_tracer_provider 目前仅在 observability.rs 内部的测试中调用（行 105），无外部调用点，安全。
- 测试矩阵：span_emission_tests mod（observability.rs:163-458）使用自写 SpanCapture subscriber，完全不依赖 OTel 类型，无需 cfg guard。noop_provider_builds 测试（行 104）需加 #[cfg(feature="otel")] guard，否则 cargo nextest run -p zhive-core 在默认特性集下会因返回类型不匹配（() vs SdkTracerProvider）而编译失败。

## tests

- cargo check -p zhive-core --lib（无任何 features）：必须通过，不能出现 opentelemetry_sdk / tracing_opentelemetry 任何类型引用。这是方案 b 的核心验收条件。
- cargo check -p zhive-core --lib --features otel：必须通过，noop_tracer_provider() 返回 opentelemetry_sdk::trace::SdkTracerProvider。
- cargo nextest run -p zhive-core（默认特性）：span_emission_tests 中的三个集成测试（run_turn_opens_zhive_turn_span / tool_call_opens_zhive_tool_call_span / compaction_opens_zhive_compaction_span）全部通过——这些测试只用 tracing::subscriber::set_default + 自写 SpanCapture，不依赖 OTel，应当在无 otel feature 下通过。
- cargo nextest run -p zhive-core --features otel：包含 noop_provider_builds 测试，全部通过。
- cargo fmt --check && cargo clippy -p zhive-core -- -D warnings（两次：无 features / --features otel）：无警告，特别注意 clippy 对 #[cfg(not(feature="otel"))] 空函数 noop_tracer_provider() -> () 的 unused_must_use 警告——stub 版本不加 #[must_use] 即可。
- doctest 验证：两个 noop_tracer_provider 版本的 # Example 块必须可编译（cargo test --doc -p zhive-core / cargo test --doc -p zhive-core --features otel）。

## risks
1. Cargo workspace optional dep 传播细节：`workspace.dependencies` 条目不能带 optional 字段——若在 workspace 侧写 optional=true，Cargo 会报错「cannot have optional workspace dependency」。optional 只能由引用该 dep 的成员 crate 声明：workspace 侧保持普通声明，zhive-core 侧写 `{ workspace = true, optional = true }`，feature 激活由 crate 的 `otel` feature 控制。这是唯一正确的写法。

2. noop_tracer_provider stub 返回 () 的语义破坏：任何当前调用 noop_tracer_provider() 并使用其返回值的代码都会编译失败。已确认（grep 结果）：目前唯一调用点是 observability.rs:105（测试 noop_provider_builds），加 #[cfg(feature=\"otel\")] 后消除。公开 API 的返回类型变化（SdkTracerProvider → ()）在语义上是破坏性变更，但 Phase 1 内部还没有外部消费方，可接受。

3. doc comment 中的类型路径引用：即使不在 cfg(feature=\"otel\") 内，doc comment 中出现 `[`opentelemetry_sdk::trace::SdkTracerProvider`]` 这样的 intra-doc link 若无 otel feature 会导致 rustdoc 警告（broken link）。解决：无 otel 版本的 stub doc comment 不使用 intra-doc link，改为普通文本说明。已在方案中体现（stub doc 不含 [`opentelemetry_sdk::...`] 引用）。

## recommendation
实装顺序（两步，单 PR 完成，约 25 行改动）：

1. 改 `crates/zhive-core/Cargo.toml`：[features] 加 otel 行；三个 OTel dep 引用加 `{ workspace = true, optional = true }`（workspace Cargo.toml 不动，optional 只在 crate 侧；共 ~5 行改动）。
2. 改 `crates/zhive-core/src/observability.rs`：noop_tracer_provider cfg 分裂 + noop_provider_builds test guard + 顶部 module doc 更新（约 20 行）。

验证命令（按顺序跑，全绿后提 PR）：
```sh
cargo check -p zhive-core --lib
cargo check -p zhive-core --lib --features otel
cargo nextest run -p zhive-core
cargo nextest run -p zhive-core --features otel
cargo fmt --check
cargo clippy -p zhive-core -- -D warnings
cargo clippy -p zhive-core --features otel -- -D warnings
```

范围建议：这是纯收口改动，不涉及任何运行时行为，改动面极小（两文件约 25 行），本阶段一次完成。不需要拆分多 PR。
