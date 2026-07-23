---
sidebar_position: 3
title: 恢复会话
---

# 恢复会话

恢复会把已记录的上下文重新载入运行时，使智能体从它上次停下的地方继续，而非从一场空对话开始。用 `agit start` 携带
智能体的上下文开始新工作，用 `agit resume` 继续某个特定的已记录会话。

## 携带智能体的上下文开始

```bash
agit start
```

这会启动一个已携带智能体最新上下文的会话，上下文来自它上次所在的任何仓库。无论你在其中运行什么，捕获都会记录。

| 标志 | 效果 |
|---|---|
| `--agent <name>` | 仅此一次调用运行某个特定智能体。它不会翻转工作树默认值，因此 `agit start --agent frontend` 与 `agit start --agent api` 可在两个终端里同时运行。 |
| `--as <runtime>` | 在 Claude Code 或 Codex 中启动。 |

若要改为给工作树设定一个默认智能体，请使用 `agit a switch <name>`。

## 继续一个已记录会话

```bash
agit resume
```

不带参数时，`agit resume` 载入活动智能体的最新会话。指名另一个则载入它：

- `agit resume <agent>` 载入那个智能体的最新会话。
- `agit resume <session-id>` 从解析出的智能体存储库中载入那个特定会话。

| 标志 | 效果 |
|---|---|
| `--as <runtime>` | 载入 Claude Code 或 Codex，而非源运行时。 |
| `--exec` | 在会话上启动运行时，而不仅是准备它并打印恢复命令。 |
| `--cwd <path>` | 在另一个工作目录中恢复。 |
| `--env <path>` | 将会话与某个特定的代码检出（另一个仓库）配对。 |
| `--relocate` | 当会话所对应的是同一项目被移动后的形态时，将会话所记录的路径重写到当前检出之上。参见[迁移会话](./relocating.md)。 |

`agit start` 在此启动一个携带最新上下文的全新运行时；`agit resume` 精确瞄准一个会话，且在没有 `--exec` 时，打印
原生恢复命令而非启动。

## 按名称恢复

当 agit 为一个按名称解析会话的运行时物化会话时，它将其命名为 `<branch-slug>-<6hex>`（例如 `feature-login-535719`），
而非一个裸 UUID。该名称是确定性的：同一个源会话总是安装到同一个名称之下，因此重新安装是覆盖而非堆叠副本。Codex 接受
这些名称（`codex resume feature-login-535719` 会载入该会话）；Claude Code 要求一个 UUID，因此 Claude 的安装保留一个
全新 UUID。

## 跨运行时按名称恢复

在一个运行时中记录的会话，经转换为另一运行时的格式后，可在后者中恢复。

```bash
agit convert <session> --to codex --write
agit convert <session> --to claude-code --write
```

不带 `--write` 时，该命令报告转换会产出什么。带 `--write` 时，它把结果作为目标运行时可恢复的会话安装，置于一个全新
id 之下。

- 同一运行时的转换是逐字节的复制。
- 跨运行时的转换把提示、回复与工具活动带过去。它会丢弃目标没有对应物的部分，例如加密的推理内容和运行时特有的工具
  编码。

参数可以是一个会话 id 或路径，或一个智能体名称（这会转换那个智能体的最新会话）。不带参数时，`agit convert --to
<runtime>` 转换活动智能体的最新会话。

`--to` 是必需的；一次没有目标运行时的转换是一个用法错误。

## 自动转换

守护进程在记录会话的同时就在运行时之间转换它们，因此你很少需要自己运行 `convert`：

```bash
agit watch --daemon
```

你也可以单独运行自动转换工作进程，而不带快照循环：

```bash
agit convert --watch
```

`agit convert --watch` 双向保持两个运行时的会话集同步，把每个新会话转换为另一方的格式。`--interval <n>` 设定轮询
间隔（秒），默认 5。在任一工作进程运行之后，在一个运行时中记录的会话总是可在另一运行时中恢复。逐运行时的细节参见
[运行时](./runtimes.md)，完整的 `agit watch` 循环参见[捕获会话](./capturing.md)。

## 让智能体的工具随之而来

一个智能体在其会话之外还捕获它的护具（其 MCP 服务器、技能与配置）。应用那份护具，使恢复出的会话拥有相同的工具：

```bash
agit harness          # 显示已捕获的护具
agit harness apply     # 将它应用到本仓库（先询问；--force 可跳过）
```
