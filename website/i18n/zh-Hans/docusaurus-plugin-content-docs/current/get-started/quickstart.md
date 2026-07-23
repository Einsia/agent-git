---
sidebar_position: 3
title: 快速上手
---

# 快速上手

从安装到恢复一次会话，随后可选地推送到中枢（Hub）。在一个代码仓库内部把它跑一遍。每一步都链接到其深入讲解的页面。

本文假设你已经安装了 `agit` 和一个运行时（runtime），并已设置好 git 身份。如果尚未如此，参见
[安装](./install.md)。

## 1. 配置好仓库

在你的代码仓库内部运行：

```bash
agit init --agent frontend
```

这会创建一个名为 `frontend` 的智能体（agent），写入一份将其绑定到本仓库的 `.agit.toml`，并在智能体的存储库上
安装一个密钥扫描（secret-scanning）钩子。提交 `.agit.toml`，这样同事在克隆时就能得到该智能体。以智能体所负责的
工作来命名它（`frontend`、`payments-api`），而不是以你自己或文件夹来命名。关于智能体和绑定是什么，参见
[核心概念](./concepts.md)。

## 2. 捕获一次会话

打开无人值守的捕获，然后开始工作：

```bash
agit watch --daemon
```

`agit watch` 会启动一个后台进程，在你工作时把新会话记录进存储库，并在 Claude Code 与 Codex 之间转换每一次会话，
使其在两者中都能恢复。设置一次，让它一直运行即可。用 `agit watch --status` 和 `agit watch --stop` 来管理它。如果
你想手动记录、而非在后台记录，请使用 `agit a snap`。参见 [捕获](../cli/capturing.md)。

现在运行一次会话：

```bash
agit start
```

`agit start` 会启动一次携带智能体上下文的会话。像平常一样工作；守护进程会记录它。

## 3. 确认它已被记录

```bash
agit a log
```

这会列出智能体的各次会话，最近的在最前：运行时、运行的时间与地点、其开场提示词，以及其工具活动。你的那次在最上方。
当会话发生分叉（divergence）时，`agit a log` 会把它们画成一棵树；参见 [分叉](../cli/divergence.md)。

## 4. 恢复它

```bash
agit resume
```

这会把智能体最近一次会话重新加载进一个运行时并继续，上下文完好无损。加上 `--as codex` 或 `--as claude-code`
可在另一种运行时中恢复。参见 [恢复](../cli/resuming.md)。

## 5. 发布到中枢（可选）

要把智能体分享给你的团队，先向一个中枢注册，然后推送：

```bash
agit identity register you
agit a push
```

`agit identity register` 会把本机的签名密钥（signing key）登记到中枢，使推送和拉取无需密码即可认证；参见
[将 CLI 连接到中枢](../integration/connect-cli-to-hub.md)。`agit a push` 会把存储库的远端记录进绑定，这样同事的
克隆就能找到该智能体。他们用 `agit a clone` 拉取它；参见 [共享](../integration/sharing.md)。

## 6. 浏览它

在浏览器中打开中枢上的该智能体，把一次会话以对话形式（而非原始 JSON）阅读。参见
[阅读会话](../hub/reading-a-session.md)。

## 接下来

- [核心概念](./concepts.md)：两个作用域、aid，以及会话究竟是什么。
- [CLI 概览](../cli/overview.md)：每一条命令。
- [合并](../cli/merging.md)：协调两个人分叉出的各自会话。
