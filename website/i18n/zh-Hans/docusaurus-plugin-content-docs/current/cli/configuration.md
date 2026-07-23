---
sidebar_position: 12
title: 配置
---

# 配置

agit 读取少量环境变量，并将其逐仓库的状态保存在几个约定俗成的文件中。这些都有可用的默认值，因此在你开始之前，
这里没有任何东西需要设置。`agit-hub` 服务器读取它自己的一套独立配置；参见[自托管配置](../self-hosting/configuration.md)。

## 环境变量

| 变量 | 用途 |
|---|---|
| `AGIT_HOME` | 智能体存储库与跨仓库状态所在之处。默认为 `~/.agit`。每个存储库位于 `$AGIT_HOME/agents/<aid>/`，而本机的签名密钥位于 `$AGIT_HOME/identity/`。 |
| `AGIT_AGENT` | 为该 shell 选定一个智能体，按名称或 aid。它的优先级低于 `--agent`，高于工作树的活动智能体（完整顺序见[核心概念](../get-started/concepts.md)）。 |
| `AGIT_HUB_URL` | 为 API 调用（身份、来源认证查询）指名中枢，并将其主机标记为「一个中枢」以供 git 凭据助手使用。未设置时，改用活动智能体的主远端。 |
| `AGIT_HUB_TOKEN` | 中枢 API 的持有者令牌（bearer token），会覆盖任何从远端 URL 解析出的凭据。 |
| `AGIT_HUB_USER` | 凭据助手用以认证的中枢账户，会覆盖在 `agit identity register` 时记住的账户。参见[认证](../integration/authentication.md)。 |
| `AGIT_ALLOW_SECRETS` | 密钥扫描的显式覆盖开关。设为 `1`（或 `true`/`yes`）可让一次提交、推送或快照在存在可疑密钥的情况下仍然通过。与 git 的 `--no-verify` 不同，它每次使用都会被披露。参见[密钥](./secrets.md)。 |
| `AGIT_LLM` | 合并综合的后端：`claude`（默认）、`codex`，或一个命令名（例如 `ollama run llama3`）。 |
| `AGIT_LLM_CMD` | 一条完整命令，经由 `sh -c` 运行，提示从 stdin 传入、结果从 stdout 传出。会覆盖 `AGIT_LLM`。 |

LLM 后端只做一件事：在 `agit a merge` 结尾综合出冲突清单（参见[合并会话](./merging.md)）。若无可用后端，`agit a
merge` 会列出未决冲突而不去解决它们，而其他每一条命令都会在没有模型的情况下运行。

`AGIT_HUB_PUBLIC_URL` 是一个由 `agit-hub` 读取的服务端变量，并非 CLI 所用。参见
[自托管配置](../self-hosting/configuration.md)。

## 文件

| 路径 | 位置 | 它是什么 |
|---|---|---|
| `.agit.toml` | 你的代码仓库，已提交 | 绑定：本仓库使用哪些智能体，以及从何处克隆它们。将它提交，队友便能获得这些智能体。 |
| `.agit/` | 你的代码仓库，被 git 忽略 | 本地的逐工作树状态，包括 `agit a switch` 设置的活动智能体指针。不共享。 |
| `agent.toml` | 智能体的存储库 | 保存 aid。客户端铸造它一次；其他任何东西都不再重写它。 |
| `.agit-allow-secrets` | 智能体的存储库 | 密钥扫描的允许列表：扫描不应标记的已知安全字符串。参见[密钥](./secrets.md)。 |
| `.agit/keybox.jsonl` | 智能体的存储库 | 密钥箱：每个接收方各持一份被封存的会话内容密钥信封。参见[加密](./encryption.md)。 |
| `$AGIT_HOME/identity/ed25519` | 你的机器 | 逐机器的 ed25519 签名密钥（私钥权限 `0600`），来源认证用它为会话签名。首次使用时铸造。参见[身份与签名密钥](./identity.md)。 |
| `$AGIT_HOME/identity/hub-account` | 你的机器 | 凭据助手用以认证的中枢账户，由 `agit identity register` 写入。`AGIT_HUB_USER` 会覆盖它。 |
| `sessions/sync/*.decisions.md` | 智能体的存储库 | 合并冲突账本：每次 `agit a merge` 的接受、自定义与延后决定，与合并转录记录相邻存放。参见[合并会话](./merging.md)。 |

你 push 到或 rebind 所针对的 URL 中的凭据，会在到达 `.agit.toml` 之前被剥除。含令牌在内的完整 URL 只留存于存储库
的本地 git 配置中。

文件内的 `agit:allow-secret` 编译指示（pragma）可将单行标记为误报，是 `.agit-allow-secrets` 允许列表的行级对应物。
两者都在[密钥](./secrets.md)中说明。
