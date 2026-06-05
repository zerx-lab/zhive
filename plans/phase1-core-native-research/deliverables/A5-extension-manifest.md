---
task: A5
title: Extension manifest schema（D-013 落地）
date: 2026-05-28
status: implemented
depends_on:
  - research/99-decisions D-012 (Hooks schema)
  - research/99-decisions D-013 (Extension manifest)
  - 红线 10（hook 必带 registered_by）
  - 红线 11（tool_call mutate 后必须重验证）
references:
  - ${PI}/packages/coding-agent/src/core/extensions/types.ts
  - ${PI}/packages/coding-agent/src/core/extensions/loader.ts
  - ${PI}/packages/coding-agent/src/core/extensions/runner.ts
  - ${PI}/packages/coding-agent/src/core/source-info.ts
  - ${PI}/packages/coding-agent/src/core/package-manager.ts
  - ${PI}/packages/coding-agent/src/modes/rpc/rpc-types.ts
---

> namespace 取 `kind: extension | prompt | skill`（与 D-013 第一版 `skill | slash_command | hook` 不同）。Pi 一手代码采用的 namespace 名就是 `extension | prompt | skill`（见 `rpc-types.ts:82`），且 **extension 本身就是 hook + tool + command + shortcut + flag + renderer 的聚合容器**，把 hook 与 slash_command 拍平成 namespace 会与 Pi 模型不一致。因此 hook/slash_command 收纳到 `extension` 的 sub-section。理由见 §3。

# A5 · Extension Manifest Schema

## 1. 参考点清单

| 主题 | 路径 | 行号 |
|---|---|---|
| `ToolDefinition` 12 字段全集 | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 426-472 |
| 工具 sourceInfo 挂载 | `${PI}/packages/coding-agent/src/core/extensions/loader.ts` | 192-199 |
| `RpcSlashCommand { name, description, source, sourceInfo }` | `${PI}/packages/coding-agent/src/modes/rpc/rpc-types.ts` | 76-85 |
| `ResourcesDiscoverEvent` + `ResourcesDiscoverResult` | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 494-506 |
| `SessionShutdownEvent` + `invalidate()` 生命周期 | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 551-557 |
| `invalidate()` 仅设 stale flag（zombie listener 反例） | `${PI}/packages/coding-agent/src/core/extensions/runner.ts` | 466-478 |
| `assertActive()` lazy 拦截 | `${PI}/packages/coding-agent/src/core/extensions/loader.ts` | 125-133 |
| `SourceInfo { path, source, scope, origin, baseDir }` | `${PI}/packages/coding-agent/src/core/source-info.ts` | 1-12 |
| `PathMetadata` + 三 scope = `user | project | temporary` | `${PI}/packages/coding-agent/src/core/package-manager.ts` | 47-52, 116 |
| `ResolvedPaths { extensions, skills, prompts, themes }` | `${PI}/packages/coding-agent/src/core/package-manager.ts` | 60-65 |
| 资源 precedence rank（4 层）| `${PI}/packages/coding-agent/src/core/package-manager.ts` | 162-177 |
| `RegisteredCommand` 带 sourceInfo | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 1061-1067 |
| `tool_call` 允许 mutate `event.input` 但 **不重验证** | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 816-830 |
| `ExtensionAPI.on(event, handler)` 注册表面 | `${PI}/packages/coding-agent/src/core/extensions/types.ts` | 1089-1126 |

---

## 2. Pi `ToolDefinition` 12 字段处理表（R5 finding #2 字段全集回答）

> 列：Pi 字段 → zhive 决定 → 备注。extension `manifest.json` 的 wire key 用 camelCase，Rust 内部类型用 PascalCase。

| # | Pi 字段（`types.ts:426-472`） | TS 类型 | zhive 决定 | manifest 字段名 | 备注 |
|---|---|---|---|---|---|
| 1 | `name` | `string` | **保留** | `name` | LLM tool name；manifest `tools[]` 元素 key |
| 2 | `label` | `string` | **保留** | `label` | UI 显示名；TUI render 用 |
| 3 | `description` | `string` | **保留** | `description` | 给 LLM；强制 ≤ 1024 字符（避免 prompt 灌水）|
| 4 | `promptSnippet` | `string?` | **保留** | `promptSnippet` | 与 Pi 行为一致（不填则不进 Available tools 段）|
| 5 | `promptGuidelines` | `string[]?` | **保留** | `promptGuidelines` | JSON 数组 |
| 6 | `parameters` | `TSchema` (TypeBox) | **保留 + 改名** | `parametersSchema` | 存 **JSON Schema**（zhive 用 `schemars` 出，跟 D-006 单 schema 源对齐）；不抄 TypeBox |
| 7 | `renderShell` | `"default" \| "self"` | **保留** | `renderShell` | 见 §6 ratatui 描述符；只在 ratatui 客户端有意义 |
| 8 | `prepareArguments` | `(args) => Static<TParams>` | **拒收** | — | Pi 用闭包做兼容 shim，**强制函数指针不能进 manifest**；zhive 改走 `parametersSchema` + `allowLooseInputs: bool` flag，由 host 在反序列化前做 best-effort 修正 |
| 9 | `executionMode` | `"sequential" \| "parallel"` | **保留** | `executionMode` | 默认 `sequential` |
| 10 | `execute` | async fn | **拒收（manifest 层）** | — | manifest 只声明，**执行体由 extension code 提供**（D-013 说 manifest 是 filesystem-discovered + model-invoked，code 路径 = manifest 同目录 `main.{rs.wasm,py,sh}` 之类，超 Phase 1 范围）。Phase 1 仅留 `entrypoint: string` 占位字段，只接 `"builtin"` |
| 11 | `renderCall` | `(args, theme, ctx) => Component` | **改名 + 重定义** | `renderCall` | Pi 是 React 组件函数，zhive 改成 **JSON 描述符（见 §6）**。manifest 只能存 declarative 字段；live 渲染靠 TUI client |
| 12 | `renderResult` | `(result, opts, theme, ctx) => Component` | **改名 + 重定义** | `renderResult` | 同上 |

**统计**：12 字段 → **保留 9**（含 1 改名：`parameters → parametersSchema`）+ **拒收 2**（`prepareArguments / execute`）+ **重定义 2**（`renderCall / renderResult` 由函数变 JSON 描述符；它们 _字段名_ 仍保留但 _语义_ 改了，故同时计 "保留 ∩ 重定义"，**实际有效保留 = 11**）。

按提交格式拍扁：
- 保留：9（其中 1 改名 `parameters → parametersSchema`）
- 改名 + 重定义：2（`renderCall / renderResult`）
- 拒收：2（`prepareArguments / execute`）

> TODO(开放项)：`entrypoint` 字段在 Phase 1 收什么值？候选 (a) `wasm:./main.wasm`（不进 deps）；(b) `cmd:./main` shell exec（D-005 不允许 in-core spawn extension）；(c) Phase 1 仅承认 **builtin** 工具，第三方 extension 推到 Phase 2。建议 (c)。

---

## 3. 三 namespace 字段定义（kind = extension | prompt | skill）

> `kind` 取值 = `extension | prompt | skill`（区别于 D-013 第一版 `skill | slash_command | hook`）。hook / slash_command / tool / shortcut / flag / message_renderer 全部作为 `kind = extension` 的 **sub-section**。理由：
> 1. Pi 的 namespace 实际是 **资源 / 发现单位**（`source: extension | prompt | skill` per `rpc-types.ts:82`），不是注册原语。
> 2. 把 hook、tool、command、shortcut、flag 全部塞进同一个 extension package 是 Pi 的工程现实（`ExtensionAPI` 接口 `loader.ts:183-326`），把 hook / slash_command 拍平成顶层 namespace 会拆散 manifest 的物理边界。
> 3. SlashCommand 在 Pi 是 extension 内的 `registerCommand`（`types.ts:1061-1067`）；强行拍成 top-level namespace 会导致同一目录下 extension manifest 与 `commands/*` 并存，发现器要爬两层。

### 3.1 目录布局（filesystem-discovered）

```
<settingSource>/                       # ~/.zhive/ | <project>/.zhive/ | <project>/.zhive.local/
├── extensions/
│   └── <name>/
│       ├── manifest.json              # kind = "extension"
│       ├── entrypoint.{wasm,..}       # Phase 1 builtin-only；第三方推 Phase 2
│       └── README.md
├── prompts/
│   └── <name>.md                      # kind = "prompt"（front-matter 即 manifest）
└── skills/
    └── <name>/
        ├── SKILL.md                   # kind = "skill"
        └── ...
```

发现器扫盘规则照 Pi `package-manager.ts:190-195` 的 `FILE_PATTERNS`：
- `extensions/*/manifest.json`
- `prompts/*.md`（front-matter 注入）
- `skills/*/SKILL.md`

### 3.2 `kind = extension` manifest

```json
// extensions/<name>/manifest.json
{
  "kind": "extension",
  "schemaVersion": "1",
  "name": "git-helper",
  "displayName": "Git Helper",
  "description": "Git workflow helpers",
  "version": "0.1.0",
  "authors": ["..."],
  "license": "...",
  "entrypoint": "builtin",

  "capabilities": {
    "hooks": true,
    "tools": true,
    "slashCommands": true,
    "shortcuts": false,
    "flags": false
  },

  "tools": [
    {
      "name": "git_blame_at",
      "label": "Git Blame",
      "description": "...",
      "parametersSchema": "{ \"type\": \"object\", \"properties\": { } }",
      "executionMode": "sequential",
      "renderShell": "default",
      "renderCall": { "kind": "preset", "preset": "command_line" },
      "renderResult": { "kind": "preset", "preset": "diff" }
    }
  ],

  "hooks": [
    {
      "event": "PreToolUse",
      "toolFilter": ["bash", "edit"],
      "priority": 0
    }
  ],

  "slashCommands": [
    {
      "name": "blame",
      "description": "...",
      "target": "tool:git_blame_at"
    }
  ],

  "shortcuts": [
    { "key": "ctrl+g b", "command": "blame" }
  ],

  "flags": [
    {
      "name": "auto-blame",
      "type": "boolean",
      "default": false,
      "description": "..."
    }
  ]
}
```

`entrypoint` Phase 1 只接 `"builtin"`，Phase 2 接 `"wasm:./main.wasm"` 等。`tools[]` 见 §2 字段表（去 `prepareArguments` / `execute`）。`renderCall` / `renderResult` 见 §6。`hooks[].event` 命名与 A4 HookEvent enum 对齐；`registeredBy` 由 host 在加载时自动注入（红线 10），不在 manifest 写。`slashCommands[].target` 与 prompt namespace 互通（可指 `prompt:templates/blame.md`）。

### 3.3 `kind = prompt` manifest（front-matter in `.md`）

```markdown
---
kind: prompt
schemaVersion: "1"
name: code-review
description: "Code review checklist"
modelInvocable: true        # 可以被 LLM 直接 invoke 还是只能 slash 调
allowedTools: ["read", "grep", "edit"]
disableInSubagent: false
---

You are reviewing code...
```

字段表：

| 字段 | 类型 | 来源 / 决定 | 说明 |
|---|---|---|---|
| `kind` | `"prompt"` | zhive | 必填 |
| `schemaVersion` | `"1"` | zhive | manifest schema 自身的版本，未来 breaking 用 |
| `name` | `string` | Pi `RpcSlashCommand.name` (`rpc-types.ts:78`) | 必填，全名空间内唯一 |
| `description` | `string?` | Pi `RpcSlashCommand.description` (`rpc-types.ts:80`) | |
| `modelInvocable` | `bool` | R5 finding #2 列表 | 默认 `false`，只能 slash 调；`true` 才会进 system prompt |
| `allowedTools` | `string[]?` | R5 finding #2 列表 | scope down；与 PermissionScope 合流 |
| `disableInSubagent` | `bool` | R5 finding #2 列表 | subagent 不可见，对应 D-008 父子继承的子缩窄 |

### 3.4 `kind = skill` manifest（front-matter in `SKILL.md`）

```markdown
---
kind: skill
schemaVersion: "1"
name: provider-contract-test
description: "Test provider compatibility"
modelInvocable: true
allowedTools: ["bash", "read", "edit"]
autoInvokeKeywords: ["provider", "contract"]
---

# Provider Contract Test
...
```

字段表：

| 字段 | 类型 | 来源 / 决定 | 说明 |
|---|---|---|---|
| `kind` | `"skill"` | zhive | 必填 |
| `schemaVersion` | `"1"` | zhive | |
| `name` | `string` | Pi `RpcSlashCommand` | |
| `description` | `string?` | Pi 同 | |
| `modelInvocable` | `bool` | Pi resources_discover skill 模型 | 默认 `true`（与 prompt 默认相反）|
| `allowedTools` | `string[]?` | 同 prompt | |
| `autoInvokeKeywords` | `string[]?` | zhive 新增 | Skill 触发关键词（Claude Code Skills 模型），LLM 自决是否进入 |

---

## 4. 三层 `settingSources` 合并规则

### 4.1 三层定义（路径布局）

| 层名 | 路径 | scope (Pi) | 提交策略 |
|---|---|---|---|
| user | `~/.zhive/` | `user` | 不入 git |
| project | `<repo_root>/.zhive/` | `project` | 入 git |
| local | `<repo_root>/.zhive.local/` | `project` | **不入 git**（覆盖 project 用） |

> Pi 只有 `user | project | temporary` 三态（`package-manager.ts:116`），没有显式 "local" 层 —— Pi 的 "local" 通过 settings flag `{ local: true }` 表达（`package-manager.ts:94-104`）。zhive **采用三目录显式分层**，因为目录边界 = git ignore 边界，比 settings flag 更不易出错。

### 4.2 优先级（precedence，越小越优先）

照搬 Pi `resourcePrecedenceRank` (`package-manager.ts:162-177`) 并扩展第 3 层：

| rank | scope | source | 含义 |
|---|---|---|---|
| 0 | project | settings | `<repo>/.zhive.local/`（显式覆盖）|
| 1 | project | settings | `<repo>/.zhive/`（项目入 git 配置）|
| 2 | project | auto | `<repo>/.zhive/<kind>/<name>` 直接扫盘 |
| 3 | user | settings | `~/.zhive/`（用户配置）|
| 4 | user | auto | `~/.zhive/<kind>/<name>` 直接扫盘 |
| 5 | package | — | 装在 `node_modules` / `cargo` registry 等的 package resource（Phase 2 才有）|

冲突时取 rank 最小者 winner，**rank 相同时按 manifest `priority` 字段降序**（priority 越大越优先），仍相等则报错（不允许沉默 first-wins）。

### 4.3 hook 通过 manifest 注册 vs settings 顶层注册的优先级（R5 finding #2 第二问回答）

**决定**：**禁止 settings 顶层裸注册 hook**。所有 hook 必须挂在某个 `kind = extension` manifest 下，由 host 在加载时自动注入 `registeredBy` 元数据（红线 10）。

理由：
- 红线 10 要求 hook event base 必带 `registeredBy: ExtensionRef`；若允许 settings 顶层注册，`registeredBy` 字段无法取值（不属于任何 extension），会出现 `registeredBy = "<settings>"` 这种特例，违反"每个 hook 必有明确 owner"。
- Pi 没有 settings 顶层 hook 这一模式（所有 hook 通过 `pi.on(event, handler)` 在 extension factory 里注册，见 `loader.ts:185-190`），无前例可循。
- 若用户要"全局 hook"，可建一个 `~/.zhive/extensions/global-hooks/manifest.json`，效果等价但 `registeredBy` 落地。

**覆盖规则**：同 event + 同 `toolFilter` 的多个 hook 按 §4.2 rank 合并；rank 较高的 hook **不覆盖** rank 较低的，而是 **依次执行**（hook chain）。`priority` 字段控制同 rank 内顺序。

> TODO(开放项)：A4 deliverable 必须定义 `HookChain` 的执行语义（fold reducer vs 短路）。当前预设：reducer 由 B6 实现，A5 不再展开。

---

## 5. `ResourcesDiscoverEvent` 是否进 Phase 1 —— **不进**

### 5.1 决定

**Phase 1 不实现 `ResourcesDiscoverEvent` 动态贡献机制**。Phase 1 资源发现 = **纯静态文件系统扫盘**（按 §3.1 目录结构 + §4.2 precedence）。

### 5.2 理由

1. **D-013 字面就是 "filesystem-discovered"**，动态贡献是 Pi 自身工程演化的扩展（`types.ts:494-506`），非协议原语。
2. **生命周期复杂度爆炸**：`ResourcesDiscoverEvent` 在 Pi 内的工作流是 `session_start → resources_discover → 收集 paths → re-scan`。如果 Phase 1 起就上，B5 hook host 必须支持 "动态扩展资源池" + "去重 + precedence 重排"，B3 persistence 还要决定动态资源是否落库 —— Phase 1 schema 落不下来。
3. **没有 P1 用例**：Phase 1 必交付里没有"extension 注册自定义 skill 目录"的需求；CLAUDE.md 红线"禁过度分割" 反过来要求"不该做的别做"。
4. **不阻塞未来**：保留 `event = "ResourcesDiscover"` 在 D-012 reserved 列表（A4 处理）。Phase 2 落地时只需在 manifest 加 `[capabilities] resources_discover = true`、`ExtensionHost` 实现 `on_resources_discover()` 回调即可，不动 wire schema。

### 5.3 Phase 1 替代方案

如果 Phase 1 内部就要让 extension 贡献额外资源路径，走 **manifest 静态声明**：

```json
{
  "kind": "extension",
  "...": "...",
  "resourceContributions": [
    { "kind": "skill", "paths": ["./bundled_skills/"] }
  ]
}
```

发现器在加载 extension 时一次性把这些路径并入扫盘队列。无 lifecycle，无回调，无 zombie listener 风险。

> TODO(开放项)：`resourceContributions` 是否要标 precedence？建议默认与 owning extension 同 rank，但用户可加 `priorityOffset` 微调。Phase 1 schema 预留字段名但不读。

---

## 6. `renderCall / renderResult` 的 zhive 编码方案

### 6.1 问题

Pi 的 `renderCall / renderResult` 是 React 组件函数（`types.ts:464-472`），输入 `(args, theme, context) => Component`，无法直接进 manifest（manifest 是 JSON，不是 JS）。Pi 的 `ToolRenderContext`（`types.ts:396-421`）还含 `invalidate` 闭包、`lastComponent` 引用、`state` 可变状态等 React 专属概念，**zhive TUI 是 ratatui，必须降级到 declarative JSON 描述符**。

### 6.2 zhive 模型：**declarative descriptor + preset 集**

manifest 里只能存 **JSON 描述符**，host 在 TUI 渲染时解析描述符 → ratatui widget。两种描述符级别：

#### Level 1：preset（强烈推荐，覆盖 80% 工具）

```json
"renderCall": { "kind": "preset", "preset": "command_line" },
"renderResult": { "kind": "preset", "preset": "diff" }
```

zhive 内置 preset 集（Phase 1 起码 5 个，后续追加）：

| preset | 用途 | 渲染示例 |
|---|---|---|
| `command_line` | bash / shell 工具 | `$ ls -la /tmp` 单行带 prompt 标 |
| `diff` | file_edit 类 | unified diff，` + ` / ` - ` 配色 |
| `file_tree` | ls / find | 树形列表，文件类型 icon |
| `text_block` | 通用文本 | 带 syntax-highlight，title 来自 `label` |
| `key_value` | 结构化对象 | 双列表格，args → values |

#### Level 2：composite（罕用，复杂 widget 才走）

```json
"renderCall": {
  "kind": "composite",
  "rows": [
    { "type": "title", "text": "Edit ${args.path}" },
    { "type": "diff", "oldField": "args.old_content", "newField": "args.new_content" }
  ]
}
```

支持的 row.type：`title | text | diff | key_value | file_path | spinner`。`spinner` 仅当 `context.argsComplete == false` 时显示。

#### 严禁的字段（Pi 有但 zhive 不抄）

- ❌ Pi `invalidate: () => void` —— ratatui frame-by-frame 重绘，无 invalidate 概念
- ❌ Pi `lastComponent: Component | undefined` —— 跨帧组件 ref，与 ratatui 模型相反
- ❌ Pi `state: TState` —— 工具自维护可变 state；zhive 改由 hook 主动推 `tool_execution_update` 事件，由 client 重渲

### 6.3 Context 字段在 zhive 的等价物

Pi `ToolRenderContext`（`types.ts:396-421`）13 字段 → zhive `ToolRenderHints`（JSON）：

| Pi 字段 | zhive | 备注 |
|---|---|---|
| `args` | ✅ `args` | 直接序列化 tool 输入 |
| `toolCallId` | ✅ `tool_call_id` | snake_case |
| `invalidate` | ❌ 拒 | ratatui 不需要 |
| `lastComponent` | ❌ 拒 | 无跨帧 ref |
| `state` | ❌ 拒 | 改走 update event |
| `cwd` | ✅ `cwd` | |
| `executionStarted` | ✅ `execution_started` | |
| `argsComplete` | ✅ `args_complete` | |
| `isPartial` | ✅ `is_partial` | |
| `expanded` | ✅ `expanded` | 用户可折叠/展开 |
| `showImages` | ✅ `show_images` | TUI 全局 flag |
| `isError` | ✅ `is_error` | |

---

## 7. 热重载 listener 生命周期策略

### 7.1 Pi 反例复盘

Pi `invalidate()` 实现 (`runner.ts:466-473` + `loader.ts:154-167`)：
- 只做一件事：**`state.staleMessage = message`**
- 任何使用旧 ctx 的 API 调用通过 `assertActive()` (`loader.ts:125-133`) 在 **被调用时** throw
- **未做**：从 EventBus / runtime 主动 unregister 已注册的 handlers / tools / commands / shortcuts

后果：旧 extension 的 handler 闭包仍被 `extension.handlers` Map 持有（`loader.ts:185-190`），仍会被 `runner.emit()` (`runner.ts:680-712`) 遍历 invoke。**虽然 invoke 时会 throw `staleMessage`，但 throw 本身被 emit 流程 `try/catch` 吃掉（`runner.ts:700-706` `emitError`）—— 即"僵尸 listener 被频繁拉起又抛错"，性能差且日志噪音大**。

这是 Pi 自承的工程债务（CLAUDE.md 引文："Pi 没完全解决 zombie listener"）。

### 7.2 zhive Phase 1 决定：**支持热重载，listener 用 scope token**

#### 决定 A：Phase 1 **支持** extension 热重载

- 用例：`/reload` 命令（与 Pi 一致，`types.ts:362-364`）
- 触发：用户手动；Phase 1 不做 fs-watch 自动重载（避免 inotify 跨平台坑）

#### 决定 B：listener 句柄用 **scope token**，不用 `Weak<dyn HookFn>`

**为什么不选 `Weak<dyn HookFn>`**：
- `Weak` 是 GC-friendly，但需要 host 持有 `Arc<dyn HookFn>`、extension 持有 `Weak`；问题是 hook **不能在调度时 just-in-time 升级 `Weak → Arc`，因为 hook fn 通常需要捕获 extension 自己的 state**。state 由 extension 的 `Arc` 持有，`Weak` 升级失败 = state 已释放 = hook 应该不再调，但 Rust 没法做 "如果 Weak 死了，从 dispatch 列表静默剔除" 这种短路（你得遍历到才知道，每次 emit 都要遍历检查）。
- 性能：每次 emit 都得 try-upgrade N 个 Weak。

**为什么选 scope token**：
- `register_hook(...)` 返回 `HookHandle`（unique opaque id），**host 端**用 `HashMap<HookHandle, BoxedHookFn>` 存。
- extension 在 `invalidate()` 时调 `host.unregister_scope(extension_id)` 一次性把它注册的所有 handle 清空。**主动撤销，不等 GC**。
- dispatch 时迭代的 Map 已无僵尸条目，零 throw、零 noise。

#### 决定 C：`ExtensionScope` 类型

```rust
// 仅草图，不是要在 crates/ 里实现
pub struct ExtensionScope {
    extension_id: ExtensionId,           // e.g. "git-helper@0.1.0"
    handles: Vec<HookHandle>,
}

impl Drop for ExtensionScope {
    fn drop(&mut self) {
        // 通过弱引用拿 host，遍历 handles 全部 unregister
        // 若 host 已死，no-op（unwinding 安全）
    }
}

// host 一侧
pub trait HookHost {
    fn register_hook(
        &self,
        scope: &mut ExtensionScope,
        event: HookEventName,
        registered_by: ExtensionRef,     // 红线 10
        handler: BoxedHookFn,
    ) -> HookHandle;

    fn unregister_scope(&self, extension_id: &ExtensionId);
}
```

- `Weak<dyn HookFn>` **不出现** 在 zhive 设计中
- `ExtensionScope` 的 `Drop` 是兜底（异常路径），正常 reload 走显式 `host.unregister_scope`

#### 决定 D：reload 的事件序

```
1. host.emit(SessionShutdownEvent { reason: "reload", ... })   // 给老 ext 最后一次 cleanup 机会
2. host.unregister_scope(old_extension_id)                     // 显式撤销所有 listener
3. drop(old_extension)                                         // ExtensionScope Drop 兜底（应是 no-op）
4. host.load_extension(new manifest)                            // 加载新版
5. host.emit(SessionStartEvent { reason: "reload", ... })       // 通知新 ext
```

zombie listener 在 step 2 已被清空，step 1 的 cleanup hook 是 best-effort（与 Pi 不同：Pi 让 cleanup 跑完后旧 handler 仍残留）。

### 7.3 Rust 类型选择小结

| 候选 | 选 / 不选 | 理由 |
|---|---|---|
| `Weak<dyn HookFn>` | ❌ | dispatch 时 try-upgrade 性能差；state 跟 hook fn 强耦合 |
| Scope token (`ExtensionScope` + `HookHandle`) | ✅ | 主动撤销，零 dispatch 开销，Drop 兜底 |
| `Arc<dyn HookFn>` + `Mutex<Vec<...>>` 永不撤销 | ❌ | 这正是 Pi 的状态，zombie 隐患 |

---

## 8. 关键问题逐条作答

### Q1 · R5 finding #2 字段全集

见 §2 表格。**保留 9 + 改名重定义 2 + 拒收 2** = 13（含 1 重定义条目额外计数），实际 manifest 字段 11。

### Q2 · Hook 通过 manifest 注册 vs settings 顶层注册的优先级

**不允许 settings 顶层裸注册 hook**。所有 hook 必须挂 manifest 下，host 自动注入 `registeredBy`（红线 10）。同事件多 hook 走 hook chain，rank（settingSources precedence）+ priority（manifest 字段）双键排序。详 §4.3。

**选 A 不选 B 的理由**：选项 B（允许 settings 顶层）违反红线 10；选项 A（manifest-only）零特例。

### Q3 · `ResourcesDiscoverEvent` 进 Phase 1？

**不进**。Phase 1 = 纯静态扫盘 + manifest 静态 `resourceContributions`（声明式，无 callback）。详 §5。

**选 A 不选 B 的理由**：B（进 Phase 1）会拖出 lifecycle、去重、zombie listener、persistence 落库四个子问题，Phase 1 schema 锁不死。A（不进）零回归代价（reserve 事件名，Phase 2 直接补）。

### Q4 · `renderCall / renderResult` 在 zhive 怎么对应

**两级 declarative descriptor**：`preset`（5 个内置预设，覆盖 80% case）+ `composite`（行列组合）。Pi 的函数式 React 组件不抄，`invalidate / lastComponent / state` 三字段拒收。详 §6。

**选 A 不选 B 的理由**：B（manifest 里塞函数指针 / wasm 渲染函数）= Phase 2 才有的 extension code 运行能力；A（declarative）= Phase 1 可静态校验、可序列化、可在多种 client（TUI / 未来 Web UI）复用。

### Q5 · 热重载 + zombie listener

**支持热重载**（手动 `/reload`，无 fs-watch）。listener 用 **scope token**（`ExtensionScope` 持 `HookHandle` 列表，`Drop` 兜底，显式 `unregister_scope` 在 reload 主路径上）。**不用 `Weak<dyn HookFn>`**（性能 + state 耦合两条理由，详 §7.2）。

**选 A 不选 B 的理由**：B（`Weak`）每次 emit O(N) try-upgrade，且 state 强耦合 fn 时 Weak 升级失败 = 静默丢事件。A（scope token）reload 主路径显式清空，dispatch 零额外开销。

---

## 9. 未决项（汇总 TODO）

> TODO(开放项-1)：`entrypoint` 字段在 Phase 1 收什么值？建议 Phase 1 仅承认 `"builtin"`，第三方 entrypoint 推 Phase 2。

> TODO(开放项-2)：A4 deliverable 必须定义 `HookChain` 的执行语义（fold reducer vs 短路）。当前预设由 B6 实现，A5 不再展开。

> TODO(开放项-3)：`resourceContributions` 的 `priorityOffset` 字段 Phase 1 是否实现？建议预留字段名但不读，避免 manifest schema 后续不兼容。

> TODO(开放项-4)：`preset` 列表是否在 zhive-proto schema 里枚举（即 client 必须支持的最小集），还是只在 TUI crate 内？倾向放 proto enum（schemars 出 schema 让所有 client 知道）。

> TODO(开放项-5)：`tool_call` hook mutate `event.input` 后强制重验证（红线 11）由 B5 host 实现；A5 manifest 层不需要新字段，但 manifest 的 `parametersSchema` 是重验证的 source-of-truth —— 强校验时 host 必须能加载该 schema，意味着 schema **必须在 manifest 静态可读**（不能 dynamic compute）。

> `disableInSubagent` 字段在 prompt / skill 以及 `tools[]` / `hooks[]` 元素上均提供，与 D-008 subagent 父子继承贯通。

> TODO(开放项-7)：schemaVersion 演进策略。建议 zhive 自己写一个 `xtask check-manifest-compat` 验证。

---

## 10. 决策落地记录

> **D-013 namespace**（记于 `decision-diffs.md` §1.11）：从第一版 `kind: skill | slash_command | hook` 改为
> > `kind: extension | prompt | skill`，其中 `extension` 内含子表 `tools[] / hooks[] / slashCommands[] / shortcuts[] / flags[]`。
>
> 理由：见 §3 开头三条（Pi 一手 namespace 名、聚合 vs 拍平、目录边界 = 物理边界）。

> **hook 注册边界**："hook 必须挂 extension manifest，不允许 settings 顶层裸注册"，与红线 10 直接联动（host 拒绝 settings 级注册）。

> **settingSources**：三层（user/project/local）的 **目录路径 + precedence rank 表** 见本 deliverable §4.1-4.2。
