---
title: 配置
parent: 中文文档
nav_order: 13
---

# 配置

agit 读取少数几个环境变量，并把它逐仓库的状态保存在几个约定俗成的文件里。所有这些都有可用的默认值，因此在开始之前无需设置任何东西。`agit-hub` 服务器读取它自己的一套变量，另在下文单独列出。

## 环境变量

| 变量 | 用途 |
|---|---|
| `AGIT_HOME` | agent 存储库和跨仓库状态存放的位置。默认 `~/.agit`。每个存储库位于 `$AGIT_HOME/agents/<aid>/`。 |
| `AGIT_AGENT` | 为 shell 选定一个 agent，按名称或 aid。它的优先级低于 `--agent`，高于工作区的活动 agent（完整的解析顺序见[概念](concepts.html)）。 |
| `AGIT_LLM` | 合并综合处理所用的后端：`claude`（默认）、`codex`，或一个命令名称（例如 `ollama run llama3`）。 |
| `AGIT_LLM_CMD` | 一条完整命令，经由 `sh -c` 运行，提示词从 stdin 传入，结果从 stdout 取出。它覆盖 `AGIT_LLM`。 |
| `AGIT_ALLOW_SECRETS` | 密钥扫描的可见覆盖开关。设为 `1`（或 `true`/`yes`）可让提交、推送或快照在有疑似密钥的情况下仍然放行。与 git 的 `--no-verify` 不同，它每次使用都会披露。参见[不让密钥进入共享历史](secrets.html)。 |

LLM 后端只做一件事：在 `agit a merge` 结束时综合出冲突清单（参见[协调分叉的会话](merging.html)）。没有可用后端时，`agit a merge` 会改为列出未决冲突而不去解决它们，其余每一条命令都无需模型即可运行。

## hub 环境变量

`agit-hub` 服务器（参见[自建 hub](../deploying-the-hub.html)）读取以下这些变量。把它们全部留空即可获得零配置的自建部署：元数据用 SQLite，二进制大对象用本地文件系统，注册为邀请制。

| 变量 | 用途 |
|---|---|
| `AGIT_HUB_DB` | 一个 `postgres://` URL 选择 Postgres 后端（用于生产）。留空（或任何非 URL 的值）则选择位于数据根下的默认 SQLite `hub.db`。 |
| `AGIT_HUB_REGISTRATION` | 设为 `1`/`true`/`open`/`yes` 时启用自助注册（`POST /api/register`）。默认关闭（邀请制）。`--open-registration` 标志的作用与此相同。 |
| `AGIT_HUB_S3_ENDPOINT` | 设置为非空时，把二进制大对象存入 S3/Garage，而非本地文件系统。它独立于 `AGIT_HUB_DB` 选择大对象后端。 |
| `AGIT_HUB_S3_BUCKET` | 存放大对象的桶。设置了 `AGIT_HUB_S3_ENDPOINT` 时必填。 |
| `AGIT_HUB_S3_ACCESS_KEY`、`AGIT_HUB_S3_SECRET_KEY` | S3 凭据。设置了 `AGIT_HUB_S3_ENDPOINT` 时必填。 |
| `AGIT_HUB_S3_REGION` | S3 区域名称。默认为 `garage`。 |

若设置了 `AGIT_HUB_S3_ENDPOINT` 却缺少桶或某个密钥值，会在启动时报错，而不会悄悄回退到本地磁盘。

## 文件

| 路径 | 位置 | 是什么 |
|---|---|---|
| `.agit.toml` | 您的代码仓库，已提交 | 绑定：本仓库使用哪些 agent，以及从哪里克隆它们。请提交它，好让同事获得这些 agent。 |
| `.agit/` | 您的代码仓库，被 git 忽略 | 逐工作区的本地状态，包括 `agit a switch` 所设的活动 agent 指针。不共享。 |
| `agent.toml` | agent 的存储库 | 保存 aid。客户端只铸造它一次；此后没有任何东西会改写它。 |
| `.agit-allow-secrets` | agent 的存储库 | 密钥扫描白名单：扫描不应标记的已知安全字符串。参见[不让密钥进入共享历史](secrets.html)。 |
| `$AGIT_HOME/identity/ed25519` | 您的机器 | 逐机器的 ed25519 签名密钥（私钥 `0600`），来源溯源用它为会话签名。首次使用时铸造。参见[验证会话的产生者](provenance.html)。 |
| `sessions/sync/*.decisions.md` | agent 的存储库 | 合并冲突记录簿：每一次 `agit a merge` 的接受、自定义和搁置决定，就在合并记录旁边。参见[协调分叉的会话](merging.html)。 |

您推送或 rebind 所针对的 URL 中的凭据，会在进入 `.agit.toml` 之前被剥除。含令牌的完整 URL 只保留在存储库本地的 git 配置里。

文件内的 `agit:allow-secret` 行内指令把单独一行标记为误报，是 `.agit-allow-secrets` 白名单在行级别上的对应物。两者都在[不让密钥进入共享历史](secrets.html)中一并说明。
