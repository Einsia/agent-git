---
title: 概念
parent: 中文文档
nav_order: 14
---

# 概念

对 agit 所用术语的简明定义。当某篇指南用到一个您想弄准的词时，请查阅此处。

## Agent

一个存放会话记录的 git 仓库。它位于 `~/.agit/agents/<aid>/`（在 `$AGIT_HOME` 之下），与您的代码分开。为 agent 命名时，按它所负责的工作命名（`frontend`、`payments-api`），而不是按某个人或某个文件夹命名。一个 agent 可以跨多个仓库工作，一个仓库也可以承载多个 agent。

## aid

一个 agent 稳定的身份，形如 `agt_<uuid>`，只铸造一次，并提交在存储库内部的 `agent.toml` 里。名称和远端 URL 都是可以变化的标签；aid 才是身份。因为 `.agit.toml` 记录了 aid，一个用相同名称重建的远端就无法悄无声息地把您绑定到另一个 agent；而合并会用 aid 来判断两边是不是同一个 agent（参见[协调分叉的会话](merging.html)）。

## 存储库

agent 的 git 仓库（会话记录本身）。`agit a <git 命令>` 会针对存储库运行 git。`agit a log` 和 `agit a diff` 为它渲染会话视图；其余大多数 `agit a` 命令都是普通的 git。

## 环境

您的代码仓库。`agit <git 命令>` 会针对它原样运行 git。agit 唯一添加的文件是 `.agit.toml`。

## 绑定（`.agit.toml`）

您代码仓库中一个已提交的文件，声明本仓库使用哪些 agent，以及从哪里克隆它们。同事的全新克隆会读取它，从而拉取相同的 agent。参见[与团队共享 agent](sharing.html)。

## 会话

一个 agent 的一次记录运行：提示词、回复、工具调用和编辑，正如运行时所转储的那样。会话就是 agit 所版本化的对象。

## 运行时

产生会话的编码工具，即 Claude Code 或 Codex。参见[在运行时之间迁移会话](runtimes.html)。

## agit 如何选定一个 agent

一条作用于某个 agent 的命令，会按以下顺序解析出它作用于哪一个：

1. 命令上的 `--agent <name>`
2. 环境中的 `$AGIT_AGENT`
3. 工作区的活动 agent，由 `agit a switch <name>` 设置
4. `.agit.toml` 中绑定的默认值

如果这些都解析不出来，命令会报错，而不是去猜。
