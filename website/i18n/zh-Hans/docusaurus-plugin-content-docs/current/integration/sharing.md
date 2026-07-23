---
sidebar_position: 3
title: 发布与获取智能体
---

# 发布与获取智能体

共享一个智能体（agent）是 git 原生的：将其存储库（store）push 到中枢（Hub），队友便可将其 clone 下来，并打开一个
已经带有其上下文的会话（session）。中枢将每个智能体托管为一个 git 仓库。这些命令可对任何能够提供该存储库服务的 git 主
机使用；中枢在此之上增加了 Web UI 与按智能体划分的权限。

对于已声明的中枢，基于密钥的认证（key-auth）覆盖本页的每一条命令，因此一旦某台机器的密钥完成注册，就无需再输入任何令
牌。参见[认证](./authentication.md)。

## 发布一个智能体

绑定一个远程，然后 push：

```bash
agit a remote add origin https://agit.anggita.org/frontend.git
agit a push -u origin HEAD
git add .agit.toml && git commit -m "declare the frontend agent"
```

`agit a push` 会对存储库执行一次真正的 git push，成功后将该存储库的来源（origin）记录到 `.agit.toml`——这是 agit
向你的代码仓库添加的唯一一个文件。URL 中的任何凭据在写入该文件之前都会被剥除；完整的 URL 保留在存储库的本地 git 配置
中。请提交 `.agit.toml`，以便队友获得该智能体并知道从何处 clone 它。

远程绑定之后，后续的 push 只需一条裸的 `agit a push`，它发送当前分支以及仅有的新会话。当绑定了多个远程时，一条裸的
`agit a push` 会推送到每一个远程；若要推送到单个指定的远程，使用 `agit a push --to <name>`。

## 获取一个智能体

已经 clone 了代码仓库的队友，已经拥有 `.agit.toml` 中的绑定。一条命令即可 clone 它所声明的每一个智能体：

```bash
agit init
agit start
```

若要 clone 单个智能体：

```bash
agit a clone frontend
```

裸名称通过 `.agit.toml` 解析；URL 则 clone 该存储库并沿用其身份。`agit a clone` 是一次智能 clone：对存储库 URL 执
行原始的 `git clone` 会生成一个解析不到任何智能体的嵌套仓库，而此命令将该存储库作为智能体沿用，携带同一身份（其
aid），而非一份副本。在默认作用域下的 `agit clone` 会检测出中枢存储库 URL 并执行同样的操作，并打印一行提示；`--git`
则强制执行原始 clone。

由于 `.agit.toml` 是队友写入的，agit 会将其中声明的远程视为不受信任，并在 clone 之前对照传输允许列表（transport
allowlist）进行检查，从而使 `ext::<cmd>` 之类的 URL 无法让 git 执行命令。

## 拉取队友的工作

```bash
agit a pull
```

当历史允许时，这会执行快进（fast-forward）。`agit a fetch` 会移动远程跟踪引用，并报告哪些会话已到达，但不将它们整合进
来。当两端发生分叉（divergence）时，pull 会拒绝操作，而不是让 git 对转录记录做基于文本的合并，并将你引导到
`agit a merge`。参见[分叉](../cli/divergence.md)与[合并](../cli/merging.md)。

## 公开存储库与私有存储库

每个智能体都有一个可见性，默认为私有，并可为成员授予读、写或管理权限。私有智能体与不存在的智能体无从区分，因此智能体名
称无法被枚举。公开存储库任何人皆可读取，包括匿名 clone；写入仍然需要授权。一次 push 或 pull 被允许执行什么，取决于你在
该智能体上的授权，而非凭据本身。参见[仓库](../hub/repositories.md)与[组织](../hub/organizations.md)。
