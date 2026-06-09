# zhive 调研档案

本目录沉淀所有为 zhive 架构决策做的调研。每篇都是带证据、可被未来 review 的原始材料。

## 索引

| 编号 | 主题 | 状态 |
|---|---|---|
| [08](./08-product-revenue-2026-06/) | **产品营收方向调研（中国大陆，2026-06）** | ✅ active — 5 角度并行调研 + 对抗核查；3 个方向按营收可能性排序 |
| [20](./20-file-revert-design/) | **文件编辑回退机制（调研 + 设计）** | ✅ active — 影子 git 主干 + 5 处经核实硬伤修正；设计完成未实现 |
| [91](./91-architecture-review-2026-05-27/) | **架构 review（2026-05-27）** | ✅ active — 7 轮 21 subagent 交叉验证 |
| [92](./92-reference-mapping/) | **外部参考项目对应关系** | ✅ active — 每个 zhive 模块→参考的外部文件/字段/PR |
| [99](./99-decisions/) | **决策汇总（R3+R4 终版）** | ✅ active — D-001~D-015 |

> 当前权威文件：[99-decisions](./99-decisions/)（权威决策）+ [91-architecture-review-2026-05-27](./91-architecture-review-2026-05-27/)（证据链）。
> 旧调研 01-06（codex-rs / opencode / Warp / 协议选型 / ConnectRPC / 编译优化）已于 2026-05-27 清理，事实快照与推翻轨迹完整保留在 91。

## 目录结构规范

**每次调研 = 一个独立目录**，不在 `research/` 根直接放 md 文件。

```
research/
├── README.md                       # 本文件，总索引
└── NN-topic/                       # 每个调研一个目录
    ├── README.md                   # 调研主文档（必需）
    ├── notes/                      # 草稿、原始笔记（可选）
    ├── sources/                    # 截图、抓包、PDF 等附件（可选）
    ├── snippets/                   # 引用的源码片段、配置示例（可选）
    └── diagrams/                   # 架构图、流程图（可选）
```

## 命名规范

- 目录名：`NN-topic`，NN 是两位数字编号，`topic` 用 kebab-case
- 数字段建议：
  - `0X` 行业先例调研（拆解既有产品）
  - `1X` 协议/接口选型
  - `2X` 引擎/runtime 设计
  - `3X` 沙箱/工具/安全
  - `4X` 部署/分发/SDK
  - `5X` 性能/可观测
  - `9X` 决策、复盘、纠错记录
- 临时调研用 `draft-NN-topic/`，定稿后改名
- 子文件命名建议：
  - 主文档统一 `README.md`
  - 附件文件名描述性，如 `sources/codex-tui-cargo-toml.txt`、`diagrams/protocol-flow.mermaid`

## 写作规范

1. **每篇 README 开头写 frontmatter**：
   ```
   ---
   topic: 简短主题
   date: YYYY-MM-DD
   status: draft | active | superseded
   supersedes: NN-xxx   # 若取代旧调研
   ---
   ```
2. **结论先行**，论据在后
3. **必须带可验证的证据**：源码链接、commit hash、版本号、benchmark 数字
4. **引用外部资料用 markdown 链接**，文末统一列 Sources
5. **避免主观断言**，决策类内容放 `99-decisions/`
6. **被推翻的旧调研不要删**，改 status 为 `superseded` 并写 supersedes 指向新文件

## 添加新调研的步骤

```bash
# 1. 在 research/ 下建新目录
mkdir research/NN-your-topic

# 2. 写 README.md（含 frontmatter）
$EDITOR research/NN-your-topic/README.md

# 3. 把附件按需放进 sources/ / snippets/ / diagrams/
# 4. 更新本文件的索引表
```

## 当前项目背景速览

- zhive 目标：类似 opencode/codex 的 agent harness
- 双形态分发：cargo install 的 TUI bin（ratatui） + 可嵌入的 Rust SDK
- 未来支持：远程沙箱、云环境、Web UI、IDE 集成
- 核心约束：编译速度优先、与 MCP/ACP/LSP 生态互操作
- 当前代码：多 crate workspace（`crates/` 下含 zhive-proto / zhive-core / zhive-client-native / zhive-tui / zhive-cli / zhive-bridge-stdio / zhive-mcp / zhive-bridge-acp，外加 xtask），默认 bin 为 zhive-cli
