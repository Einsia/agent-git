---
title: 与团队共享 agent
parent: 中文文档
nav_order: 6
---

# 与团队共享 agent

共享 agent 是 git 原生的：添加一个远端并推送即可。此后，同事就能克隆该 agent，并打开一次已经带有其上下文的会话。您可以推送到任何为存储库提供服务的 git 托管处；若需要网页界面和逐 agent 的权限控制，请运行 [hub](../hub.html)。

## 把 agent 推送到远端

```
agit a remote add origin https://hub.example.com/frontend.git
agit a push -u origin HEAD
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` 会对存储库执行一次真正的 git push，并在推送时把存储库的远端记录进 `.agit.toml`。URL 中的任何凭据都会在写入该文件前被剥除；完整 URL 只保留在存储库本地的 git 配置里。请提交 `.agit.toml`，好让同事获得该 agent 并知道从哪里克隆它。

远端一经绑定，之后的推送就只是一句 `agit a push`，它只发送当前分支和新增的会话。当绑定了不止一个远端时，一句 `agit a push` 会推送到每一个远端。用 `agit a push --to <name>` 只推送到某个指定的远端。

如果某个 hub 因认证失败而拒绝了推送，agit 会指引您前往它的令牌页面。

## 让同事就绪

已经克隆了代码仓库的同事，手上就已经有 `.agit.toml` 绑定。一条命令即可让他就绪：

```
agit init            # 克隆 .agit.toml 声明的每一个 agent
agit start           # 打开一次带有该 agent 上下文的会话
```

若只想克隆其中一个 agent，而非全部：

```
agit a clone frontend
```

裸名称会通过 `.agit.toml` 解析。无论哪种方式，得到的都是同一个 agent，带着相同的身份（它的 aid），而不是一份副本。用 `agit harness apply` 把它的工具一并带过来（参见[恢复会话](resume.html)）。

## 拉取同事的工作

```
agit a pull
```

当历史允许时，这会做快进合并。当两边出现分叉时，它会停下并引导您使用 `agit a merge`。参见[协调分叉的会话](merging.html)。

## agent 随身携带了什么

代码仓库中的 `.agit.toml` 记录了本仓库使用哪些 agent，以及从哪里克隆它们。它是 agit 向您代码仓库添加的唯一文件，并且会被提交。agent 的身份（它的 aid）存在存储库内部，因此一个用相同名称重建的远端，无法悄无声息地把您绑定到另一个 agent。参见[概念](concepts.html)。

因为 `.agit.toml` 是别人写的，agit 会把它声明的远端视为不可信。在克隆 `.agit.toml` 中的某个远端之前，agit 会对照一份传输方式白名单进行检查，因为像 `ext::<cmd>` 这样的 URL 否则会让 git 执行 `<cmd>`。
