---
sidebar_position: 9
title: 身份与签名密钥
---

# 身份与签名密钥

每台机器持有一把 ed25519 签名密钥。agit 用它为本机捕获的会话签名，从而把一个会话与产生它的机器绑定起来。将那把密钥
登记到某个中枢账户下，就把「由此密钥签名」变为「由此人签名」，而同一把已登记的密钥正是基于密钥的中枢认证所使用的。

## 机器签名密钥

该密钥是一对 ed25519 密钥对，在首次使用时创建，存放于 `$AGIT_HOME/identity/` 之下（私钥 `ed25519` 权限为
`0600`）。它是逐机器的，而非逐智能体：一把密钥为本机捕获的一切签名。显示它：

```bash
agit identity show          # 本机的 ed25519 + x25519 公钥、存放位置、登记状态
agit provenance key         # 仅签名公钥
```

`agit identity show` 还会报告本机的 X25519 公钥，那是用来把会话内容封存给作为接收方的你的加密一半。参见
[加密](./encryption.md)。

## 签名给你带来什么

当 agit 捕获一个会话时，它用这把密钥为其签名，并将签名记录进该会话已提交的附属文件（sidecar），与转录记录摘要、
智能体的 aid、你的提交者邮箱以及开始时间一并存放。验证会重新算出摘要、重建被签名的消息并核对签名：

```bash
agit provenance verify <session>
```

自我验证证明内容完好且签名与所记录的密钥相符。它并不说明这把密钥属于谁，正因如此其裁决是「已验证（verified）」而
从不是「已信任（trusted）」。未签名的会话报告为「未验证（unverified）」且从不阻断；一个存在却核对不通过的签名则是
一次硬性的、非零退出的失败。会话被归属到的人，是你的 git 提交者身份，agit 以 git 的方式解析它。完整的裁决表参见
[来源认证](../integration/provenance.md)。

## 将本机登记到中枢

`agit identity register` 发布本机的公钥，好让一个中枢账户为它们背书。它离线运行：它派生 ed25519 与 X25519 的公钥
一半、对一条登记消息自签名，并打印一段可粘贴的文本块。没有任何秘密离开机器。

```bash
agit identity register you
```

输出是一段单行 JSON 文本块，加上说明：

```
{"ed25519_pub":"...","x25519_pub":"...","epoch":...,"enroll_sig":"...","label":"..."}

paste this into the hub: Account -> Signing keys -> Add a signing key
```

将该文本块粘贴进中枢的 web UI，即可把密钥登记到你的账户下。`--label <name>` 为该设备命名；不带它时 agit 会挑选一个
默认名。登记还会在本地记住该中枢账户，好让 git 凭据助手知道该以哪个账户认证。

检视已登记的内容：

```bash
agit identity show           # 本机的密钥及其登记状态
agit identity show alice     # 另一账户已登记的设备密钥，来自中枢
agit identity keys           # 本机的密钥详情
agit identity revoke <fpr-or-label>
```

一旦你机器的密钥完成登记、且你的提交者邮箱映射到你的账户，`agit provenance verify` 就会升级为 `VERIFIED AS <you>`。
中枢的签名密钥页面在[签名密钥](../hub/signing-keys.md)中说明。

## 同一把密钥也做密钥认证

已登记的 ed25519 密钥也是你面向私有中枢的凭据。`agit a push`、`agit a pull`、`agit a fetch` 与 `agit a clone` 会
把 agit 接为面向中枢主机的 git 凭据助手：该助手通过应答一个挑战、用这把密钥铸造一个短时令牌，于是你无需粘贴令牌
就能推送、拉取和克隆一个私有存储库。你在此登记的密钥，正是为那些挑战签名的密钥。参见
[认证](../integration/authentication.md)。
