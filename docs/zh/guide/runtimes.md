---
title: 在运行时之间迁移会话
parent: 中文文档
nav_order: 5
---

# 在 Claude Code 与 Codex 之间迁移会话

agit 支持两个运行时：Claude Code 和 Codex。在其中一个里记录的会话，可以转换成另一个的格式并在那里恢复。守护进程会替您完成这件事；您也可以手动转换会话。

## agit 如何选择运行时

读取会话的命令，会使用您用 `--from` 指明的运行时。如果您没有指明，而此处只有一个运行时留有会话，agit 就用那一个。如果两个都有，它会询问用哪一个。列出 agit 识别的运行时：

```
agit adapter
```

会话是按运行时分别存储的，因此一次 Claude Code 会话和一次 Codex 会话在 agent 的存储库里并排存放。`snap`、`merge` 以及其他会话命令都接受 `--from claude-code` 或 `--from codex` 来指定其中一个。

## 手动转换会话

```
agit convert <session> --to codex --write
agit convert <session> --to claude-code --write
```

不加 `--write` 时，命令只报告这次转换会产生什么。加上 `--write` 时，它会把结果安装为目标运行时可以恢复的会话。安装后的会话会得到一个全新的 id —— 目标运行时需要它才能恢复这次会话。

- 同一运行时之间的转换是逐字节的复制。
- 跨运行时的转换会把提示词、回复和工具活动搬过去。它会丢弃目标运行时没有对应物的内容，例如加密的推理过程和运行时专有的工具编码。

参数可以是会话 id 或路径，也可以是 agent 名称（此时转换该 agent 的最新会话）。不带参数时，`agit convert --to <runtime>` 转换当前活动 agent 的最新会话。

## 自动转换

守护进程在记录会话的同时就会在运行时之间转换，因此您很少需要亲自运行 `convert`：

```
agit watch --daemon
```

此后，一个运行时里记录的会话总能在另一个运行时里恢复。守护进程的细节参见[捕获 agent 会话](capture.html)；关于 `--from` 在合并时如何选择复活会话的运行时，参见[协调分叉的会话](merging.html)。
