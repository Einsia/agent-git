---
sidebar_position: 13
title: 命令参考
---

# 命令参考

每个原生 `agit` 动词，分类列出，附一行说明。未列出的一切都会透传给 git：`agit <git-args>` 在代码仓库上运行，
`agit a <git-args>` 在解析出的智能体存储库上运行。关于作用域选择符如何工作，参见[概览](./overview.md)。

## 初始化与顶层

| 命令 | 作用 |
|---|---|
| `agit init [--agent <name>]` | 准备本仓库：克隆 `.agit.toml` 声明的智能体（或用 `--agent` 铸造第一个），并在存储库上安装密钥钩子。 |
| `agit clone <target> [--git] [--no-switch]` | git 的 clone，但对 agit-hub 智能体存储库更智能：被明确识别的中枢存储库 URL，或已知的智能体名称，会被采纳为一个智能体。`--git` 强制执行原始的 git 克隆。 |
| `agit --version` | 打印 agit 版本。 |
| `agit help`（亦即 `-h`、`--help`） | 打印顶层用法。 |

## 捕获

| 命令 | 作用 |
|---|---|
| `agit a snap [<runtime>] [--from <rt>] [--no-harness] [--watch] [--interval <n>]` | 将本项目的会话转储（及护具）镜像进存储库并提交，受密钥扫描把关。除非指名一个运行时，否则捕获所有存在的运行时。 |
| `agit watch [--daemon\|--background] [--stop] [--status] [--no-convert] [--no-harness] [--interval <n>]` | 无人值守：监视两个运行时，双向自动快照与自动转换。`--daemon` 让它在后台运行。 |

参见[捕获会话](./capturing.md)。

## 恢复与转换

| 命令 | 作用 |
|---|---|
| `agit start [--agent <name>] [--as <runtime>]` | 启动一个已携带智能体最新上下文的会话。 |
| `agit resume [<session\|agent>] [--as <rt>] [--cwd <path>] [--env <path>] [--relocate] [--exec]` | 将一个已记录的会话载入运行时并继续它。 |
| `agit convert [<session\|agent>] --to <rt> [--from <rt>] [--cwd <path>] [--write]` | 将会话改写为另一运行时的格式。`--watch [--interval <n>]` 运行自动转换工作进程。 |
| `agit harness [show\|apply] [--from <rt>] [--from-env <path>] [--force]` | 显示或应用已捕获的 MCP 服务器、技能与配置。`apply` 会先征询确认。 |
| `agit adapter` | 列出 agit 已知的运行时。 |

参见[恢复会话](./resuming.md)与[运行时](./runtimes.md)。

## 调和

| 命令 | 作用 |
|---|---|
| `agit a merge <target> [--from <rt>] [--both] [--quick] [--splice] [--dry-run]` | 通过对话方式将两段已分叉的记忆调和为一个可恢复的合并会话。`sync` 是别名。 |
| `agit a log [--raw\|--git]` | 将存储库的会话渲染为一张分叉 DAG，最新在前。`--raw` 回退为普通 `git log`。 |
| `agit a diff [<from>] [<to>] [--raw\|--git]` | 两个引用之间新增的提示与编辑，而非逐字节 diff。`--raw` 回退为普通 `git diff`。 |

参见[合并会话](./merging.md)与[分叉](./divergence.md)。

## 共享（智能体存储库）

| 命令 | 作用 |
|---|---|
| `agit a push [<remote>\|<url>] [git-push-args]` | 推送存储库的会话并将远端记录进 `.agit.toml`。先行扫描；遇到中枢认证被拒时，会指向令牌页面。 |
| `agit a pull` | 快进式拉取；出现分叉时转向 `agit a merge`。 |
| `agit a fetch` | 获取，并报告哪些会话已到达。 |
| `agit a clone [--init] [--no-switch] <name\|url>` | 按身份克隆一个智能体的存储库；裸名称会经由 `.agit.toml` 解析。`--init` 将一个全新智能体铸造进空存储库。 |

参见[共享一个智能体](../integration/sharing.md)。

## 管理智能体（`agit a`）

| 命令 | 作用 |
|---|---|
| `agit a init <name>` | 铸造一个新智能体（一个带有自身身份的存储库）并将它绑定到本仓库。 |
| `agit a list` | 你本地拥有的智能体，附会话数、监视器状态，以及哪个处于活动状态。 |
| `agit a status` | 逐仓库的概览：智能体、活动的那个、会话数、最近活动，以及活动存储库相对其远端的状态。 |
| `agit a switch <name>` | 选定本工作树的活动智能体。 |
| `agit a info <name>` | 某个智能体的名称、aid、存储库路径与远端。 |
| `agit a rename <old> <new>` | 重命名一个智能体（aid 不变）。 |
| `agit a rebind [--remote <url>] [--new-id]` | 修复一个绑定的身份，或给一个分叉赋予它自己的 aid。 |
| `agit a commit [git-commit-args]` | 提交进存储库，先扫描已暂存的索引。 |

## 密钥

| 命令 | 作用 |
|---|---|
| `agit a scan [--staged] [<file>…]` | 手动扫描会话转储中的密钥。 |
| `agit a purge-history [--yes]` | 带护栏的历史重写，清除明文（或重新封存会话）；打印强制推送命令，从不自动推送。 |

参见[密钥](./secrets.md)。

## 加密

| 命令 | 作用 |
|---|---|
| `agit a encrypt [--readers a,b] [--public] [--team] [--org <org>] [--yes]` | 启用逐会话密钥箱加密，面向指名的接收方。 |
| `agit a encrypt --export <file>` / `--import <keyfile>` / `--rotate` | 管理机器全局的对称密钥（无中枢的部署）。 |
| `agit a readers add\|rm\|ls <user>\|--public\|--team [--key HEX] [--repin]` | 管理一个会话的密钥箱接收方。 |
| `agit a rekey` | 轮换内容密钥并将其重新封存给当前接收方。 |
| `agit crypt unlock` | 将本机的内容密钥从已提交的密钥箱恢复进本地密钥环。 |
| `agit a escrow enable` | 选择加入中枢辅助的密钥托管（仅限处于中枢辅助模式的组织下）。 |

参见[加密](./encryption.md)。

## 身份与来源认证

| 命令 | 作用 |
|---|---|
| `agit identity register <you> [--label <name>]` | 打印一段可粘贴的文本块，用于将本机密钥登记到某个中枢账户下。 |
| `agit identity show [<user>]` | 本机的密钥与登记状态，或另一账户已登记的设备密钥。 |
| `agit identity keys` | 本机的密钥详情。 |
| `agit identity revoke <fpr-or-label>` | 吊销一个已登记的密钥。 |
| `agit identity pin <user> [--repin] [--key HEX]` | 固定（或重新固定）一个账户已注册的密钥集。 |
| `agit provenance verify [<session\|agent>] [--repin]` | 对照会话所记录的密钥，检查一个已捕获会话的签名。 |
| `agit provenance key` | 显示本机的签名公钥。 |

参见[身份与签名密钥](./identity.md)与[来源认证](../integration/provenance.md)。

## 诊断

| 命令 | 作用 |
|---|---|
| `agit doctor` | 快速健康检查与环境摘要，可粘贴进 bug 报告。 |
| `agit debug [--out <dir>] [--rerun "<subcmd>"]` | 写出一份完整的、经脱敏的诊断包。不上传任何内容。 |
| `agit relocate [<session>] [--to <path>] [--yes]` | 将在错误目录下启动的会话迁移进本仓库。 |

参见[诊断](./diagnostics.md)与[迁移会话](./relocating.md)。

## 工作区与 shell

| 命令 | 作用 |
|---|---|
| `agit workspace [log]` | 显示智能体—环境的配对。 |
| `agit workspace restore [N]` | 将两个仓库一同回滚到某次配对的联合状态。 |
| `agit graph` | 显示工作区—状态时间线及关系边。 |
| `agit shadow [install\|uninstall\|status]` | 在你的 shell 中让 `git` 经由 `agit` 转发（bash/zsh/fish/PowerShell）。 |
| `agit hub team rekey\|sync <org>` | 轮换或重新封存一个组织的 Team KEK 给其成员。 |

## 由 git 调用的辅助命令

这些命令由 git 调用，从不手动输入：`agit credential <get\|store\|erase>`（中枢凭据助手）、
`agit hook-scan`（pre-commit/pre-push 扫描钩子），以及 `agit crypt-clean` / `agit crypt-smudge`
/ `agit crypt-purge-index`（加密过滤驱动）。
