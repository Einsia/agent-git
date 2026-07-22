---
title: 快速上手
parent: 中文文档
nav_order: 2
---

# 快速上手

本指南带您从安装走到第一次记录会话，全程约五分钟。在一个代码仓库内只需运行一次。此后，agit 会记录该 agent 产生的每一次会话。

## 1. 安装 agit

```
npm install -g @einsia/agentgit
```

这会安装 `agit` 客户端。确认它已在 PATH 中：

```
agit --version
```

## 2. 设置 git 身份

agit 会把每一次记录的会话归属到您的 git 身份。它不会替您编造身份，并且在身份未设置时拒绝记录会话。像对待任何 git 仓库那样，设置一次即可：

```
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

## 3. 为仓库创建一个 agent

在您的代码仓库内运行：

```
agit init --agent frontend
```

这会创建一个名为 `frontend` 的 agent，写入一份把它与仓库绑定的 `.agit.toml`（请提交它，好让同事也获得该 agent），并在 agent 的存储库上安装密钥扫描钩子。为 agent 命名时，按它所负责的工作命名（`frontend`、`payments-api`），而不是按您本人或按文件夹命名。

每个仓库只运行一次 `agit init`。若要向已经设置好的仓库再添加一个 agent，使用 `agit a init <name>`。

## 4. 开启自动捕获

```
agit watch --daemon
```

这会启动一个后台进程，在您工作时把新会话记录进 agent 的存储库，并把每一次会话在 Claude Code 与 Codex 之间互相转换，使一次会话无论在哪个工具里记录，都能在另一个工具里恢复。设置一次，让它一直运行即可。

```
agit watch --status    # 显示它是否在运行，以及已捕获了什么
agit watch --stop      # 停止它
```

## 5. 开始工作

```
agit start
```

这会启动一次已经载入 agent 上下文的会话。照常工作即可，守护进程会随手记录这次会话。

## 6. 确认会话已被记录

```
agit a log
```

这会按时间从近到远列出 agent 的会话：运行时、运行时间、运行地点、起始提示词以及工具活动。您这次会话会出现在最上方。当会话出现分叉时，`agit a log` 会把它们绘制成一棵树。

## 后续步骤

- [捕获 agent 会话](capture.html)：agit 记录了什么，以及如何用 `agit snap` 手动捕获。
- [与团队共享 agent](sharing.html)：把 agent 推送到远端，让同事可以克隆它。
- [协调分叉的会话](merging.html)：合并两个人在同一 agent 上的工作。
- [命令参考](command-reference.html)：每一条命令。
