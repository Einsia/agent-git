---
title: 恢复会话
parent: 中文文档
nav_order: 4
---

# 恢复会话

恢复会把 agent 记录下来的上下文载入运行时，让 agent 从上次中断处继续，而不是从一段空白的对话重新开始。用 `agit start` 借助 agent 的上下文开展新工作，用 `agit resume` 继续某一次已记录的会话。

## 借助 agent 的上下文开展工作

```
agit start
```

这会启动一次会话，其中已经带有本仓库中该 agent 的上下文。您在其中运行的一切，守护进程都会记录。

- `--agent <name>` 运行指定的 agent。选择是逐条命令生效的，因此 `agit start --agent frontend` 与 `agit start --agent api` 可以在两个终端里同时运行。若想改为给工作区设置一个默认 agent，使用 `agit a switch <name>`。
- `--as <runtime>` 选择 Claude Code 或 Codex。参见[在运行时之间迁移会话](runtimes.html)。

## 继续一次已记录的会话

```
agit resume
```

不带参数时，`agit resume` 会载入当前活动 agent 的最新会话。若要继续另一次，请指明它：

- `agit resume <agent>` 载入该 agent 的最新会话。
- `agit resume <session-id>` 从解析出的 agent 的存储库中载入指定的那一次会话。

选项：

| 选项 | 结果 |
|---|---|
| `--as <runtime>` | 把会话载入 Claude Code 或 Codex。 |
| `--exec` | 直接在会话上启动运行时，而不仅仅是把它准备好。 |
| `--cwd <path>` | 在另一个工作目录中恢复。 |
| `--env <path>` | 把会话与指定的代码检出配对。 |
| `--relocate` | 把会话记录中的路径改写到当前检出上。 |

## 让 agent 的工具一并带过来

一个 agent 会把它的运行环境（它的 MCP 服务器、技能和配置）连同会话一起捕获。把这套运行环境应用到当前仓库，恢复出来的会话就能拥有相同的工具：

```
agit harness          # 显示捕获到的运行环境
agit harness apply     # 把它应用到本仓库
```
