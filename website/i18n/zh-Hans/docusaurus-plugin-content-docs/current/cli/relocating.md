---
sidebar_position: 11
title: 迁移会话
---

# 迁移会话

一个会话是为它所运行的仓库而捕获的。如果某个运行时把会话记录在了错误的工作目录下（一个父目录或 monorepo 目录，
或同一项目在别处的另一份检出），那个会话就被搁浅了：它本属于本仓库，却被写在另一路径之下。`agit relocate` 检测被
搁浅的会话，并将它们迁移进当前仓库，好让捕获不再搁浅它们。

## 错误 cwd 警告

当 agit 注意到一个运行在本仓库根之外目录的会话时，它会发出警告，而非默默地把它捕获到错误的 slug 之下。警告会指出
运行时、会话被记录进的目录，并指向 `agit relocate` 以将其纳入。这正是为什么你原以为会在 `agit a log` 中看到的一个
会话可能缺席：它被记录在了别处，正等待被迁移。

## 迁移

```bash
agit relocate
```

裸形式会列出每一个运行在别的目录、却本属于本仓库的会话，然后在迁移它们之前询问：

```
Sessions that ran elsewhere but belong in /home/you/code/web:
  claude-code · /home/you/code · 2 hours ago
      "add a rate limiter"
bring these 1 session(s) into /home/you/code/web? [Y/n]
```

| 标志 | 效果 |
|---|---|
| `<session>` | 迁移一个会话，按 id、转录记录路径，或其所记录目录的一个子串匹配。 |
| `--to <path>` | 覆盖目的地。它必须是一棵 git 工作树；会话会安装到一个由它派生的 slug 之下。 |
| `--yes`（`-y`） | 跳过确认。 |

目的地默认为本仓库的根。迁移会把历史移入存储库，因此它从不无人值守地进行：一个没有 `--yes` 的非交互 shell 会被当作
「否」。在正确的位置运行它、且没有任何被搁浅的会话，是一个有效且完整的结果，而非错误。

## 相关

`agit resume --relocate` 处理相邻的情形：当一个已记录会话所对应的是同一项目被移动后的形态时，针对当前检出继续它，
并将会话所记录的路径重写到该检出之上。参见[恢复会话](./resuming.md)。
