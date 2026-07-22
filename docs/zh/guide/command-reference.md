---
title: 命令参考
parent: 中文文档
nav_order: 12
---

# 命令参考

凡未在此列出的，都会透传给 git：`agit <git-args>` 作用于代码仓库，`agit a <git-args>` 作用于解析出的 agent 的存储库。

## 处理会话

| 命令 | 作用 |
|---|---|
| `agit init [--agent <name>]` | 设置本仓库：克隆 `.agit.toml` 声明的各个 agent，或用 `--agent` 铸造第一个。 |
| `agit start [--agent <name>] [--as <runtime>]` | 启动一次已带有该 agent 上下文的会话。 |
| `agit snap [--from <runtime>]` | 手动把本项目的会话捕获进存储库：在一个带关卡的步骤里镜像并提交（疑似密钥会镜像到磁盘，但挡在历史之外）。 |
| `agit watch [--daemon\|--stop\|--status] [--no-convert]` | 无人值守地自动 snap 与自动转换；`--daemon` 让它在后台运行。 |
| `agit convert <src> --to <runtime> [--write]` | 把一次会话改写成另一个运行时的格式。 |
| `agit resume <src> [--as <runtime>] [--exec]` | 把一次会话载入运行时并继续它。 |
| `agit adapter` | 列出 agit 认识的运行时。 |
| `agit harness [apply]` | 显示或应用某个 agent 捕获到的 MCP 服务器、技能和配置。 |
| `agit shadow install\|uninstall\|status` | 在您的 shell（bash/zsh/fish/PowerShell）中让 `git` 经由 `agit` 路由。 |
| `agit a scan` | 手动扫描 agent 的会话，查找密钥。 |
| `agit provenance key` | 显示本机的签名身份（一把 ed25519 密钥，首次使用时铸造）。 |
| `agit provenance verify <session>` | 自我验证一次捕获会话的签名。参见[验证会话的产生者](provenance.html)。 |
| `agit identity register <you>` | 打印一段可粘贴的块，把本机密钥登记到您的 hub 账户下。 |
| `agit identity show` | 显示本机的密钥指纹和登记状态。 |

用 `agit watch --daemon` 一次性设置好捕获：它在您工作时为新会话拍快照，并在运行时之间转换它们，使两个 CLI 都能恢复。`agit snap` 是手动的替代方案，它像守护进程一样在一个带关卡的步骤里镜像并提交。

`agit a commit`、`agit a push` 以及每一次快照，都会在把工作交给 git 之前于进程内扫描内容中的密钥，因此即便 git 自己的钩子被跳过，扫描依然生效。可见的覆盖方式是 `AGIT_ALLOW_SECRETS=1`；参见[不让密钥进入共享历史](secrets.html)。

## 管理 agent（`agit a`）

| 命令 | 作用 |
|---|---|
| `agit a list` | 您本地拥有的 agent，附带会话数以及哪个处于活动状态。 |
| `agit a status` | 逐仓库概览：本仓库使用哪些 agent、哪个处于活动状态、每个的会话数、最近活动、实时监视器状态，以及活动存储库相对其远端的状况（未推送、落后或分叉）。 |
| `agit a init <name>` | 向本仓库再添加一个 agent（一个带有自己身份的存储库）。 |
| `agit a switch <name>` | 选定本工作区的活动 agent。 |
| `agit a clone <name\|url>` | 按身份克隆某个 agent 的存储库；裸名称通过 `.agit.toml` 解析。 |
| `agit a info <name>` | 某个 agent 的名称、aid、存储库路径和远端。 |
| `agit a rename <old> <new>` | 重命名一个 agent。 |
| `agit a log [--raw\|--git]` | 把存储库的会话作为时间线呈现，从近到远：运行时、时间、运行地点、起始提示词及其工具活动。`--raw`（或 `--git`）退回到对存储库执行普通的 `git log`。 |
| `agit a diff [<from>] [<to>] [--raw\|--git]` | 两个 ref 之间的会话级变化：新增的提示词与编辑，而非对会话记录字节的逐行差异。不带 ref 时，使用本仓库的未推送范围。`--raw`（或 `--git`）退回到普通的 `git diff`。 |
| `agit a push [<remote>\|<url>]` | 推送存储库的会话，并把远端记录进 `.agit.toml`（凭据已剥除）。不带 refspec 时推送当前分支（全新的存储库无需 `-u`）；不带远端时散发到每一个已绑定的远端，并逐一指名。会先扫描密钥；当 hub 因认证拒绝推送时，会指向 hub 的令牌页面。 |
| `agit a pull` | 快进拉取；出现分叉时导向 `agit a merge`。 |
| `agit a fetch` | 获取，并报告哪些会话到达了。 |
| `agit a rebind [--remote <url>] [--new-id]` | 修复一份绑定的身份，或给一个分叉赋予它自己的 aid。参见[迁移](migration.html)。 |
| `agit a merge <target> [--from <runtime>] [--both] [--quick] [--splice] [--dry-run]` | 通过对话把两段记忆协调成一次可恢复的合并会话。`--splice` 跳过模型，仅把两边合并成一次会话；`--dry-run`（别名 `--preview`）显示合并将会做什么但并不执行。参见[协调分叉的会话](merging.html)。 |

`agit a log` 和 `agit a diff` 默认渲染存储库的会话视图，因为在那里执行裸的 `git log`/`git diff` 会是一堵会话记录字节的墙。`--raw`（或 `--git`）是退回真正 git 的逃生口，因此脚本化的 `--format` 输出仍然可用。
