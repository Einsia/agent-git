---
sidebar_position: 4
title: 运行时
---

# 运行时

agit 支持两个运行时：Claude Code 和 Codex。每个都以自己的格式把实时会话写入自己的目录；agit 读取那些转储，将它们
镜像进存储库，再把它们写回，好让任一 CLI 都能恢复另一方产生的会话。列出 agit 识别的运行时：

```bash
agit adapter
```

## agit 如何挑选运行时

一条读取会话的命令，使用你用 `--from` 指名的运行时。若你不指名，且此处只有一个运行时存在会话，agit 就用那一个。
若两者都有，它会询问。会话是逐运行时存储的，因此一个 Claude Code 会话和一个 Codex 会话在智能体的存储库中并肩而立，
而 `snap`、`merge`、`convert` 及其他会话命令都接受 `--from claude-code` 或 `--from codex` 来挑选其一。

## Claude Code

Claude Code 把每个会话写成一份位于逐项目目录下的单一 JSONL 转录记录：

```
~/.claude/projects/<project-slug>/<uuid>.jsonl
```

项目 slug 由工作目录派生，因此会话按项目切分。agit 读取那份转录记录，而当它为 Claude Code 安装一个待恢复的会话时，
它按相同布局、置于当前检出的 slug 之下写回。Claude Code 通过扫描该目录并匹配 id 来解析会话，因此没有需要维护的索引：

```bash
claude --resume <uuid>
```

Claude Code 要求一个 UUID 形式的 id，且没有可设置的标题字段，因此 agit 把 Claude Code 会话安装到一个全新 UUID 之下
（而非它能给 Codex 的那种可读的 `<branch>-<hex>` 名称）。

## Codex

Codex 把会话写成按日期分区的 rollout 文件：

```
~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl
```

agit 读取这些文件，并在安装时把一个 rollout 文件写在某个日期目录之下；确切日期无关紧要，因为 Codex 的 resume 会
递归扫描 `sessions/` 并按 id 解析。Codex 通过其 `session_meta` 中记录的 `cwd` 认领一个会话，因此一个嵌入了另一项目
父会话的 fork/resume rollout 会被过滤掉，绝不会被误认为本项目的会话。

以交互方式恢复一个 Codex 会话：

```bash
codex resume <session-id>
```

`codex resume [SESSION_ID] [PROMPT]`（提示可选）会打开携带该会话的 TUI，这正是 `agit start` 和 `agit resume` 所要
的。非交互的 `codex exec resume <id>` 要求一个提示，因此一次无提示的启动会在那里失败；agit 使用 `codex resume`。

### 恢复时的 model_provider

`codex resume` 从会话的 `session_meta` 读取模型提供方（model provider）以引导客户端。当 agit 把一个会话转换为 Codex
格式时，它写入 `model_provider: "openai"`（Codex 的默认值），于是 Codex 随后挑选该提供方的默认模型。一次同厂商的
Codex-到-Codex 重放会保留源提供方，而非强制为 openai。要为非 openai 后端在启动时覆盖提供方：

```bash
codex resume <session-id> -c model_provider=<x>
```

## 在运行时之间转换

在一个运行时中记录的会话，经转换后可在另一运行时中恢复。守护进程会自动完成这件事；你也可以手动转换。关于 `agit
convert`、`agit convert --watch`，以及一次跨运行时转换携带与丢弃什么，参见[恢复会话](./resuming.md)。关于 `--from`
如何在一次调和中选择复活会话的运行时，参见[合并会话](./merging.md)。
