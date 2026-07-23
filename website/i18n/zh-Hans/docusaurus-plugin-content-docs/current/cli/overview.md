---
sidebar_position: 1
title: CLI 概览
---

# CLI 概览

`agit` 是一个与 git 兼容的 CLI，它像 git 为代码做版本管理那样，为智能体（agent）的会话历史做版本管理。它
同时封装了两个仓库：

- **环境（Environment）**：即你的代码仓库。`agit <git-args>` 会原样对它执行 git。
- **智能体存储库（Agent Store）**：一个独立的、存放会话转录记录的 git 仓库，位于 `$AGIT_HOME` 之下。`agit a
  <git-args>`（`agit agent` 的别名）会对它执行同构的 git 操作。

作用域选择符是 `agit` 之后的第一个记号。`agit a commit` 会提交到存储库；`agit commit -a` 则是对代码仓库执行原生
git，其中 `-a` 是 git 的暂存全部（stage-all）选项。由于 `a` 是子命令而非标志，二者不能互换位置。旧的 `agit -a
<args>` 标志作为已弃用的别名保留下来。

凡是 `agit` 不识别为原生动词的内容，都会透传给所选作用域上的 git。因此 `agit a log`、`agit a add -A`、`agit a
push`、`agit a diff` 都能正常工作，只有那些能带来额外价值的动词才会被拦截。

## 命令面

原生命令按其功能分组。

### 捕获（Capture）

将运行时（runtime）的实时会话记录进存储库。

- `agit a snap` 镜像并提交当前会话，受密钥扫描（secret scan）把关。
- `agit watch` 全程无人值守运行：自动快照（auto-snap）加自动转换（auto-convert），可在前台运行，也可作为后台守护进程运行。

参见[捕获会话](./capturing.md)。

### 恢复（Resume）

将已记录的上下文重新载入运行时。

- `agit start` 启动一个已携带智能体最新上下文的全新会话。
- `agit resume` 继续某个特定的已记录会话。
- `agit convert` 将会话改写为另一运行时的格式，使任一 CLI 都能恢复它。

参见[恢复会话](./resuming.md)与[运行时](./runtimes.md)。

### 调和（Reconcile）

让已分叉（divergence）的历史重新汇合。

- `agit a pull` 执行快进（fast-forward），出现分叉时转向合并。
- `agit a merge` 通过对话方式调和两个已分叉的会话。
- `agit a log` 渲染分叉的 DAG。

参见[合并会话](./merging.md)与[分叉](./divergence.md)。

### 共享（Share）

发布存储库并把团队的工作拉回本地。

- `agit a push` / `agit a pull` / `agit a fetch` 通过共享远端在成员间搬运会话。
- `agit a clone` 按身份克隆一个智能体。

参见[共享一个智能体](../integration/sharing.md)与[将 CLI 连接到中枢](../integration/connect-cli-to-hub.md)。

### 密钥（Secrets）

让凭据不进入共享历史。

- `agit a scan` 扫描会话转储中的密钥。
- 提交、推送、合并这三道关卡都会在内容进入 git 之前先行扫描。
- `agit a purge-history` 将密钥从过往提交中重写清除。

参见[密钥](./secrets.md)。

### 加密（Encryption）

对静态存储的会话内容加密。

- `agit a encrypt` 启用逐会话的密钥箱（keybox）。
- `agit a readers` / `agit a rekey` 管理接收方并轮换密钥。

参见[加密](./encryption.md)。

### 身份（Identity）

为会话签名，并使用密钥向中枢认证。

- `agit identity register <you>` 将本机的密钥登记到某个中枢账户下。
- `agit provenance verify` 检查会话由谁产生。

参见[身份与签名密钥](./identity.md)与[认证](../integration/authentication.md)。

### 诊断（Diagnostics）

检视安装状态并生成报告。

- `agit doctor` 执行一次快速的健康检查。
- `agit debug` 写出一份经脱敏处理的诊断包。
- `agit relocate` 把在错误目录下捕获的会话迁移进本仓库。

参见[诊断](./diagnostics.md)与[迁移会话](./relocating.md)。

## 完整清单

每个原生动词及其一行说明，都收录在[命令参考](./command-reference.md)中。
关于环境变量和磁盘上的文件，参见[配置](./configuration.md)。
