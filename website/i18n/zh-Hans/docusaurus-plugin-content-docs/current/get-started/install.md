---
sidebar_position: 2
title: 安装
---

# 安装

先安装 `agit` CLI，然后确认你的 `PATH` 上有一个可用的运行时（runtime）。中枢（Hub，`agit-hub`）是一台独立的
服务器；使用 CLI 并不需要安装它。若要运行一个中枢，参见
[部署中枢](../self-hosting/deploying.md)。

## 安装 CLI

### 通过 npm 安装

```bash
npm install -g @einsia/agentgit
```

这会全局安装 `agit` 客户端。

### 从源码构建

本项目使用 Rust（edition 2024）。构建需要 `cargo >= 1.78`。

```bash
git clone https://github.com/Einsia/agent-git
cd agent-git
./build.sh --release
```

`build.sh` 会检查工具链并运行 release 构建。生成的二进制文件位于
`target/release/agit`；把它放到你的 `PATH` 上。

## 安装一个运行时

agit 记录来自编码智能体运行时的会话。请至少安装其中一个，并确认它在你的 `PATH` 上：

- **Claude Code**（`claude-code`）
- **Codex**（`codex`）

读取会话的命令会使用你用 `--from` 指定的运行时；若未指定，则使用当前唯一存在的那一个；若都无法确定，命令会询问你。
如果你想在两者之间迁移会话，就把它们都装上；参见 [运行时](../cli/runtimes.md)。

## 验证

```bash
agit --version
```

```
agit 0.2.1
```

然后检查你的环境是否配置正确：

```bash
agit doctor
```

`agit doctor` 会打印一份快速的健康检查：你的 `PATH` 上有哪些运行时、你的 git 身份、当前激活智能体的存储库与其
远端的对比，以及是否有 watch 守护进程正在运行。参见 [诊断](../cli/diagnostics.md)。

:::note
agit 会把每一次记录下来的会话归属到你的 git 身份，并在该身份未设置时拒绝记录。如果
`agit doctor` 报告没有身份，请像对待任何 git 仓库一样，一次性设置好它：

```bash
git config --global user.name  "Your Name"
git config --global user.email you@example.com
```
:::

## 接下来

- [快速上手](./quickstart.md)：记录、恢复并发布你的第一次会话。
- [核心概念](./concepts.md)：开始之前的思维模型。
