---
title: 中文文档
nav_order: 1
has_children: true
---

[English](../index.html) | 中文

# agit

agit 会把 AI agent 产生的每一次编码会话（agent 读取、运行和修改了什么）保存进一个 git 仓库，让会话像代码一样被版本化、共享与协调。它可与 Claude Code 和 Codex 配合使用。

`agit <git 命令>` 会针对您的代码仓库原样运行 git。在 `agit` 后面加上 `a`，同一条命令便改为针对 agent 的存储库运行 —— 那是另一个独立的 git 仓库，存放会话记录（一个 agent 就是一段记忆：存放会话记录的 git 仓库）。因此 `agit log` 显示代码历史，`agit a log` 显示 agent 的会话。

## 您想做什么？

| 目标 | 指南 |
|---|---|
| 安装 agit 并记录第一次会话 | [快速上手](guide/quickstart.html) |
| 工作时自动记录会话 | [捕获 agent 会话](guide/capture.html) |
| 载入上下文后继续一次会话 | [恢复会话](guide/resume.html) |
| 在 Claude Code 与 Codex 之间迁移会话 | [在运行时之间迁移会话](guide/runtimes.html) |
| 把某个 agent 及其历史交给同事 | [与团队共享 agent](guide/sharing.html) |
| 合并两个人各自分叉的会话 | [协调分叉的会话](guide/merging.html) |
| 阻止密钥进入共享历史 | [不让密钥进入共享历史](guide/secrets.html) |
| 确认某次会话由哪个人产生 | [验证会话的产生者](guide/provenance.html) |
| 在网页界面中浏览 agent 与会话 | [在 hub 上浏览 agent](hub.html) |
| 为团队运行一个 hub | [自建 hub](deploying-the-hub.html) |
| 让 agent 指向重建的远端或分叉 | [重新绑定 agent 的身份](guide/migration.html) |

## 参考

- [命令参考](guide/command-reference.html)：每条命令一行说明。
- [配置](guide/configuration.html)：环境变量与文件。
- [概念](guide/concepts.html)：术语表（agent、aid、存储库、环境）。

## 安装

```
npm install -g @einsia/agentgit
```

首次运行请参阅[快速上手](guide/quickstart.html)。
