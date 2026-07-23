---
sidebar_position: 4
title: 端到端来源认证
---

# 端到端来源认证

来源认证（provenance）将一次被捕获的会话（session）与产生它的机器、以及拥有该机器密钥的人绑定起来。每台机器都用自
己的 ed25519 密钥签署它所捕获的会话。要把“由此密钥签名”变为“由此人签名”，需将该密钥注册到某个中枢（Hub）账户，并对
照中枢进行验证。

## 签名如何工作

当 agit 捕获一次会话时，它用本机密钥对其签名，并将签名记录到该会话已提交的边车（sidecar）文件中，与转录记录
（transcript）摘要、智能体（agent）的 aid、你的提交者（committer）邮箱以及起始时间并列存放。公钥可以安全共享；签
名随会话一同存放在存储库（store）中。

会话被归属于谁，取决于你的 git 提交者身份，其解析方式与 git 的解析方式一致。像对任何仓库那样设置一次即可：

```bash
git config --global user.email you@example.com
git config --global user.name  "Your Name"
```

在你的身份未设置时，agit 拒绝记录会话，因为会话正是被归属的对象。

显示本机的签名密钥：

```bash
agit provenance key
```

## 自我验证

```bash
agit provenance verify [<session|agent>]
```

不带参数时，它验证当前活动智能体的最新会话。传入会话路径或 id 则验证那一个；传入智能体名称则验证该智能体存储库中的每
一次会话。验证会重新计算转录记录摘要、重建被签名的消息，并对照记录中携带的公钥核验签名。

| 判定 | 含义 | 退出码 |
|---|---|---|
| verified | 内容完整且签名与其记录的密钥相符 | 0 |
| unverified, no signature | 会话不携带签名（在启用签名之前捕获，或当时无可用密钥） | 0 |
| tampered | 转录记录在签名之后被更改：其当前摘要与被签名的摘要不一致 | 非零 |
| bad signature | 存在签名但无法对照其记录的密钥通过核验 | 非零 |

未签名的会话报告为“unverified”，从不阻断。存在但无法通过核验的签名则是硬性失败。自我验证证明内容完整且签名与记录的
密钥相符。它并不说明该密钥归属于谁，这正是判定为“verified”而绝非“trusted”的原因。

## 作为一个人来验证

将本机密钥注册到你的中枢账户：

```bash
agit identity register you
```

这会打印一个可粘贴的注册（enroll）密钥块。将其粘贴到 Web UI 中 Account 下的 Signing keys 以注册该密钥。参见
[签名密钥](../hub/signing-keys.md)与 [`agit identity`](../cli/identity.md)。

在密钥已注册的情况下，`agit provenance verify` 会在给出判定之前，先到中枢的注册表中查找该提交者邮箱。当你的邮箱映射
到某个中枢账户、且该账户已注册的密钥包含会话的签名密钥时，判定会升级：

| 判定 | 含义 | 退出码 |
|---|---|---|
| VERIFIED AS `<user>` | 自我验证通过，且提交者邮箱映射到某账户，该账户已注册的密钥包含该签名密钥；这是唯一的“作为一个人被验证”的判定 | 0 |
| signed, unregistered | 自我验证通过，但该邮箱未映射到任何账户（或没有可达的中枢）；归属于一个密钥，尚未归属于一个人 | 0 |
| KEY MISMATCH | 自我验证通过且该邮箱映射到某账户，但该签名密钥不在该账户已注册的任何密钥之中：可能是伪造 | 非零 |

## 为何这些判定难以伪造

中枢将会话归属于其已注册密钥完成签名的那个账户，并以提交者邮箱作为查找句柄进行绑定。有两条性质保证其诚实性：

- 账户的已注册密钥集合会在首次见到时被固定（pin）。中枢此后无法偷换一枚密钥来制造虚假的 VERIFIED AS；集合一旦被更
  改会导致失败，而不会悄然重新归属。在第二台机器上注册的密钥仍然会被判定为 VERIFIED AS 你本人，而不会误判为
  KEY MISMATCH。

  当某个被固定的密钥因合法轮换而变化时，验证会阻断并打印重新固定（re-pin）的指令。请通过带外方式确认新的指纹，然后
  用 `--repin` 接受它：

  ```bash
  agit provenance verify <session> --repin
  ```
- 对照已注册密钥的签名才是真正的边界。错误或被伪造的提交者邮箱绝不会产生虚假的 VERIFIED AS：它会降级为
  “signed, unregistered”或 KEY MISMATCH。

在没有可达中枢的离线验证中，判定会降级为“signed, unregistered”，而绝不会产生虚假归属。中枢会在会话页面上渲染同一枚
徽章；参见[阅读会话](../hub/reading-a-session.md)。
