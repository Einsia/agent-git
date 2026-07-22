---
title: 捕获 agent 会话
parent: 中文文档
nav_order: 3
---

# 捕获 agent 会话

agit 会把 agent 产生的每一次会话记录进 agent 的存储库 —— 一个位于 `~/.agit` 之下的 git 仓库。记录下来的会话包含完整记录：您给出的提示词、agent 的回复、它运行的工具，以及它编辑的文件。您可以用守护进程自动捕获会话，也可以用 `agit snap` 手动捕获。

## agit 记录了什么

在您工作时，Claude Code 和 Codex 各自会把完整会话写进自己的目录。agit 把这份会话转储镜像进 agent 的存储库并提交。除了那份已提交的 `.agit.toml` 绑定文件之外，您代码仓库中的任何内容都不会改变。

每一次会话都会归属到您的 git 身份，因此请在捕获前设置一次（参见[快速上手](quickstart.html)）。身份未设置时，agit 会拒绝记录会话。

## 用守护进程自动捕获

```
agit watch --daemon
```

这会启动一个后台进程，它监视两个运行时，并在您工作时做两件事：

- 把每一次新会话记录进 agent 的存储库。
- 把每一次会话在 Claude Code 与 Codex 之间转换，使一个工具里记录的会话可以在另一个工具里恢复。参见[在运行时之间迁移会话](runtimes.html)。

设置一次，让它一直运行即可。您无需再运行任何捕获命令。

| 命令 | 结果 |
|---|---|
| `agit watch --daemon` | 在后台启动守护进程。 |
| `agit watch --status` | 显示它是否在运行，以及已捕获了什么。 |
| `agit watch --stop` | 停止它。 |
| `agit watch` | 在前台运行，而非在后台。 |
| `agit watch --no-convert` | 只记录会话，跳过运行时转换。 |

## 用 `agit snap` 手动捕获

如果您不想运行守护进程，可用下面的命令捕获当前会话：

```
agit snap
```

这会把运行时当前的会话文件镜像进存储库，并一步完成提交。它会像守护进程一样先运行密钥扫描。当此处两个运行时都有会话时，用 `--from claude-code` 或 `--from codex` 指定其中一个；否则 `agit snap` 会捕获在本仓库中留有会话的每一个运行时。

被疑为密钥的内容会镜像到磁盘，但在您处理它之前不会进入 git 历史。参见[不让密钥进入共享历史](secrets.html)。

## 查看记录了什么

```
agit a log            # agent 的会话，从近到远
agit a status         # 本仓库的 agent、会话数、最近活动、监视器状态
agit a diff           # 自上次推送以来新增的提示词与编辑
```

`agit a log` 和 `agit a diff` 渲染的是存储库的会话视图。加上 `--raw`（或 `--git`）可退回到对存储库执行普通的 `git log` 或 `git diff`。
