---
sidebar_position: 2
title: 捕获会话
---

# 捕获会话

捕获会将某个智能体（agent）产生的会话记录进该智能体的存储库。一个已记录的会话保存完整的转录记录（transcript）：
你给出的提示、智能体的回复、它运行的工具，以及它编辑过的文件。你可以用 `agit a snap` 手动捕获，也可以让 `agit
watch` 持续运行以不间断地捕获。

## 会被提交的内容

Claude Code 和 Codex 在你工作时各自把实时会话写入各自的目录。一次捕获会将那份会话转储镜像进智能体的存储库并提交，
连同智能体的护具（harness，即它的 MCP 服务器、技能与配置，其中密钥已脱敏）一并提交。除了被提交的 `.agit.toml`
绑定之外，你的代码仓库不会发生任何变化。

每个被提交的会话都归属到你的 git 提交者身份。一次存储库提交承载一个会话，而没有提交者的会话永远无法被归属，因此在
你的身份未设置时，agit 拒绝执行捕获：

```bash
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

agit 自身的簿记提交（铸造一个智能体、一次重命名）由 `agit@local` 署名，从不需要你的身份，因此一台新机器可以在你
尚未设置身份之前就创建智能体。关于归属所依托的签名机制，参见[身份与签名密钥](./identity.md)。

每次捕获都会先运行密钥扫描器（secret scanner）。可疑的密钥会被镜像到磁盘，但在你处理它之前会被挡在 git 历史之外。
参见[密钥](./secrets.md)。

## 手动快照

```bash
agit a snap
```

这会将运行时（runtime）当前的会话文件镜像进存储库，并在同一道受把关的步骤中提交它们。若未指名运行时，snap 会捕获
本仓库中所有存在会话的运行时。当两者都存在时，请指名其一：

```bash
agit a snap codex
agit a snap --from claude-code
```

| 标志 | 效果 |
|---|---|
| `--from <runtime>` | 捕获某一个运行时。裸位置参数（`agit a snap codex`）是同一效果的简写。 |
| `--no-harness` | 只捕获会话；跳过 MCP/技能/配置构成的护具。 |
| `--watch` | 对指名的运行时持续运行 snap（见下文）。 |
| `--interval <n>` | `--watch` 的轮询间隔（秒），默认 5。 |

## 自动快照

`agit a snap --watch` 轮询某一个运行时的转储，并在每个新会话出现时对其快照。指名一个运行时则监视该运行时；未指名、
同时覆盖两个运行时的循环则是 `agit watch`（见下文）。

```bash
agit a snap --from codex --watch
```

## 用 `agit watch` 全程无人值守

```bash
agit watch
```

`agit watch` 是完全无人值守的路径。它监视两个运行时的转储，并在你工作时做两件事：

- 将每个新会话自动快照进存储库。
- 在 Claude Code 与 Codex 之间自动转换每个会话，从而使一方记录的会话可在另一方中恢复。参见[运行时](./runtimes.md)。

它可在前台运行，也可作为后台守护进程运行：

| 命令 | 结果 |
|---|---|
| `agit watch` | 在前台运行。 |
| `agit watch --daemon`（别名 `--background`） | 在后台长期运行。 |
| `agit watch --status` | 报告守护进程是否在运行以及它已捕获了什么。 |
| `agit watch --stop` | 停止守护进程。 |
| `agit watch --no-convert` | 只自动快照；跳过运行时转换。 |
| `agit watch --no-harness` | 只捕获会话；跳过护具。 |
| `agit watch --interval <n>` | 轮询间隔（秒），默认 5。 |

设置一次后便可任其运行；你无需再次运行任何捕获命令。`agit a list` 与 `agit a status` 会为带有活动监视器的智能体
加以标注。

## 检视已捕获的内容

```bash
agit a log            # 该智能体的会话，最新在前
agit a status         # 本仓库的智能体、会话数、最近活动、监视器状态
agit a diff           # 自上次 push 以来新增的提示与编辑
```

`agit a log` 与 `agit a diff` 渲染存储库的会话视图。传入 `--raw`（或 `--git`）可回退为普通的 `git log` 或 `git
diff`。关于 `agit a log` 如何渲染分支会话，参见[分叉](./divergence.md)。
