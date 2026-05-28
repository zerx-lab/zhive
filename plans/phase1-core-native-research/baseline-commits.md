---
date: 2026-05-28
purpose: 锁定调研期间参考 repo 的 commit，防止 upstream 漂移
rule: 调研期间禁止 `git pull`；如必须升级，本文件追加一笔附"为什么升 + 受影响的 deliverable"
---

# 调研基线 commit 锚点

| Repo | 本地路径 | 锚点 commit | 一行说明 |
|---|---|---|---|
| codex | `~/Desktop/code/github/codex` | `6111791d0b` | Treat refresh_token_reused 400s as relogin-required (#24830) |
| acp-rust-sdk | `~/Desktop/code/github/acp-rust-sdk` | `2e8c815` | chore(deps): bump EmbarkStudios/cargo-deny-action from 2.0.18 to 2.0.19 (#181) |
| rmcp | `~/Desktop/code/github/rmcp` | `c330fed` | fix: reject init header/body version mismatch (#853) |
| tower-lsp | `~/Desktop/code/github/tower-lsp` | `49e1ce5` | Implement support for client-initiated $/progress |
| rusqlite | `~/Desktop/code/github/rusqlite` | `f2bc708` | Prepare next release |
| pi | `~/Desktop/code/github/pi` | `b85bf656` | fix(coding-agent): restore diff code block highlighting |

## 升级记录

（空，调研期间如需升级在此追加：`日期 / repo / 旧 → 新 commit / 受影响 deliverable / 原因`）
