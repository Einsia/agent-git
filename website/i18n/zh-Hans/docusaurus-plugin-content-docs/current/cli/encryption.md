---
sidebar_position: 8
title: 加密
---

# 加密

加密使会话内容在静态存储时不可读。存储库提交的是密文，只有已登记的接收方才能解密。这与[签名密钥](./identity.md)是
分开的：签名密钥证明会话由谁产生（来源认证）；加密控制谁能读取会话。不同的密钥，不同的职责。

## 密钥箱

agit 用一把逐会话的对称**内容密钥（content key，CK）**加密每个会话的内容。CK 从不以明文提交。取而代之，存储库提交
一个**密钥箱**（`.agit/keybox.jsonl`），其中为每个接收方各持一份被封存的 CK 信封。每个信封将 CK 封装给某接收方的
**X25519** 公钥：一对全新的临时密钥对、一次对该接收方密钥的 ECDH、一次 HKDF-SHA256，以及一次 XChaCha20-Poly1305
封存。只有持有匹配 X25519 私钥的人，才能重新算出共享密钥并打开 CK。

转录记录本身由一对 git clean/smudge 过滤器封存与解封，因此对你而言工作树是明文，而一切被提交和推送的内容都是密文。

## 启用加密

```bash
agit a encrypt
```

不带任何标志时，这是零配置路径：它将一把逐会话内容密钥铸造进一个仓库本地的密钥环，并提交一个密钥箱，其默认接收方
集合遵循该会话所归属的作用域（在组织下为团队可读，对个人所有者则显式指定）。也可直接指名接收方：

| 调用方式 | 接收方 |
|---|---|
| `agit a encrypt --readers alice,bob` | 指名的中枢账户。 |
| `agit a encrypt --public` | 任何人（一个公开节，所有人可读）。 |
| `agit a encrypt --team` | 所属组织当前的 Team KEK。可与 `--readers`/`--public` 组合。 |
| `agit a encrypt --team --org <org>` | 显式指名组织。 |

`--yes`（`-y`）以非交互方式确认。

## 将队友登记为接收方

接收方的 X25519 公钥是其[中枢身份](./identity.md)的加密一半，与其 ed25519 签名密钥一同登记。一旦队友用 `agit
identity register` 完成登记，就把他们加入某会话的密钥箱：

```bash
agit a readers add alice
agit a readers add --public
agit a readers add --team
```

添加一个读者是一次 O(1) 的密钥箱追加：它把已有的 CK 封装给新接收方的密钥，无需重新加密转录记录。移除一个则是一次
真正的轮换，因为被移除的读者仍持有旧 CK：

```bash
agit a readers rm alice
agit a readers ls          # （也可写作裸的 `agit a readers`）
```

`agit a readers add` 接受 `--key HEX` 以直接提供一把接收方密钥，接受 `--repin` 以接受一把已变更的已注册密钥。

## 轮换内容密钥

```bash
agit a rekey
```

`agit a rekey` 铸造一把新的内容密钥，将其重新封存给当前接收方集合（若曾设置公开节则予以保留），并轮换密钥环。在移除
一个读者之后使用它，或按计划定期使用。

## 在新机器上恢复密钥

当你克隆或拉取一个其密钥箱包含你的存储库时，将本机的内容密钥从已提交的密钥箱恢复进仓库本地的密钥环，好让过滤器
能够解密：

```bash
agit crypt unlock
```

这会打开封存给你 X25519 密钥的信封，并将恢复出的内容密钥写入本地密钥环。它会自行解析活动智能体。

## 清除加密前的明文

启用加密会封存今后产生的数据块，但更早的提交仍持有明文。将它从历史中重写清除：

```bash
agit a purge-history
```

`agit a purge-history` 在当前密钥环下重新加密 `sessions/**` 的每一个历史修订版本，使任何提交中都不残留加密前的明文。
它带护栏：它检查逐会话的前置条件、要求一棵干净的工作树、在可用时使用 `git filter-repo`（否则回退到 `git
filter-branch`），且从不自动推送。它会打印你审阅重写之后应运行的确切强制推送命令。`agit a encrypt --purge-history`
是同一命令的别名。

## 机器全局密钥（无中枢的部署）

对于没有中枢的存储库，`agit a encrypt` 还支持一把你在带外（out of band）共享的、机器全局的对称密钥：

| 调用方式 | 效果 |
|---|---|
| `agit a encrypt --export <file>` | 将密钥导出到文件，以便与队友共享。 |
| `agit a encrypt --import <keyfile>` | 导入一把共享的密钥。 |
| `agit a encrypt --rotate` | 铸造一把新的当前密钥并重新加密工作树；退役的密钥留在本地，因此历史仍可解密。 |

一次轮换之后，你必须 `agit a push` 并用 `--export` 重新共享新密钥；若需清除轮换前的数据块，再执行 `agit a
purge-history`。
