---
sidebar_position: 4
title: 核心概念
---

# 核心概念

agit 背后的思维模型。读一遍即可；文档的其余部分都以此为前提。

## 两个作用域

每一条 `agit` 命令都会针对两个仓库之一运行 git。紧跟在 `agit` 之后的那个词决定针对哪一个。

- **`agit <git command>`** 针对你的**代码仓库**（即工作环境）运行 git，行为不变。agit 向其中添加的唯一文件是
  `.agit.toml`。因此 `agit log`、`agit status` 和 `agit commit` 的行为与 git 完全一致。
- **`agit a <git command>`** 针对**智能体的存储库**运行 git，那是一个独立的、存放会话转录记录的 git 仓库。因此
  `agit a log` 显示智能体的各次会话，`agit a commit` 则提交到该存储库。

`a` 是一个子命令，而非一个标志（flag），所以不能调换位置：`agit a commit` 作用于存储库，而 `agit commit -a`
是 git 在代码仓库上的“暂存全部”。两者之间的差别不止一个空格。

## 智能体（Agent）

一个**智能体（agent）**就是一个存放会话转录记录的 git 仓库。它位于 `$AGIT_HOME/agents/<aid>/`（默认为
`~/.agit/agents/<aid>/`），与你的代码相分离。以智能体所负责的工作来命名它（`frontend`、`payments-api`），而不是
以某个人或某个文件夹来命名。一个智能体可以跨多个仓库工作，一个仓库也可以承载多个智能体。

## 存储库（Store）

**存储库（store）**是智能体的 git 仓库，也就是转录记录本身。`agit a <git command>` 针对它运行 git。
`agit a log` 和 `agit a diff` 会将其渲染为会话视图；其余大多数 `agit a` 命令都是在存储库上执行的纯粹 git 操作。

## aid

**aid** 是一个智能体稳定的身份标识，形如 `agt_<uuid>`，一次性铸造并提交在存储库内部。名称与远端 URL 都是可以
变更的标签；aid 才是身份本身。由于 `.agit.toml` 记录了 aid，一个以相同名称重新创建的远端无法悄然把你绑定到另一个
智能体，而一次[合并](../cli/merging.md)也会依据 aid 来判定两侧是否为同一个智能体。

## 绑定（`.agit.toml`）

**绑定（binding）**是你代码仓库中一个已提交的文件，它把一个或多个 aid 与本仓库关联起来，并记录从何处克隆每个
智能体的存储库。提交它，这样同事的全新克隆就能读取它并拉取相同的智能体。参见 [共享](../integration/sharing.md)。

## 会话（Session）

一次**会话（session）**是智能体的一次被记录下来的运行：提示词、回复、工具调用和编辑，按运行时（runtime）转储出的
原样。会话就是 agit 所版本化的对象。中枢会把一次会话渲染为可读的对话；参见
[阅读会话](../hub/reading-a-session.md)。

## 运行时（Runtime）

一个**运行时（runtime）**是产生会话的那个编码工具，即 **Claude Code**（`claude-code`）或 **Codex**
（`codex`）。agit 可以把一次会话从一种运行时转换到另一种，使其在两者中都能恢复；参见
[运行时](../cli/runtimes.md)。

## agit 如何选定一个智能体

一条作用于某个智能体的命令，会按以下顺序解析出究竟是哪一个：

1. 命令上的 `--agent <name>`
2. 环境中的 `$AGIT_AGENT`
3. 工作树（worktree）当前激活的智能体，由 `agit a switch <name>` 设定
4. `.agit.toml` 中绑定的默认智能体

如果以上都无法解析出结果，命令会报错，而不会去猜测。

## 接下来

- [快速上手](./quickstart.md)：在一次首次运行中体会这些概念。
- [CLI 概览](../cli/overview.md)：完整的命令集。
- [配置](../cli/configuration.md)：`$AGIT_HOME`、`$AGIT_AGENT` 以及其他各项设置。
