---
title: 重新绑定 agent 的身份
parent: 中文文档
nav_order: 15
---

# 重新绑定 agent 的身份

一个 agent 以它的 aid 为键，而 agit 会拒绝把一份绑定连接到 aid 与 `.agit.toml` 所记录的不符的存储库（参见[概念](concepts.html)）。这项检查能阻止一个重建的远端在同一名称下悄无声息地换入另一个 agent。当您确实是想改变这个映射时，`agit a rebind` 会覆盖它。它有两种形式。

## 让绑定指向一个重建的远端

当某个名称解析到的存储库所持有的 aid 与绑定所记录的不同时，就用 `--remote` —— 例如远端以全新身份重建，或 DNS 把该名称指向了新的地方。解析默认会拒绝这种情况；rebind 就是您接受它的方式：

```
agit a rebind frontend --remote https://hub.example.com/frontend.git
```

绑定条目会被改写为存储库实际持有的 aid，存储库的 origin 也会被设为该 URL。与 `agit a push` 一样，URL 中的任何凭据都不会进入已提交的 `.agit.toml`。

## 给一个分叉赋予它自己的身份

一个分叉的克隆会带着源头的 aid，因此它读起来是同一个 agent（同一身份的第二个主张者）。`--new-id` 会铸造一个全新的 aid，使这个分叉成为一个独立的 agent：

```
agit a rebind --new-id
```

重新铸造会移动存储库，因为存储库以 aid 为键。由此带来两个后果：

- 当有监视器正针对该 agent 运行时，此操作会被拒绝。请先用 `agit watch --stop` 停止监视器。
- 绑定到旧 aid 的其他仓库不会跟随这个分叉。每个仓库都必须再次对分叉运行 `agit a clone` 才能接上它。
