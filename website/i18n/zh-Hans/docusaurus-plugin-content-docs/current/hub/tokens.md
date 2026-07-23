---
sidebar_position: 6
title: 令牌
---

# 令牌

令牌（token）是用于 git 和脚本的凭据。一个人用密码登录并持有会话 cookie，而一个自动化客户端则发送令牌。在 Web 界面中创建一个令牌，把它用作 git 密码或 bearer 头部，用完后将其吊销。

设备密钥可以自动铸造令牌，因此你也许永远无需手动创建；参见 [认证](../integration/authentication.md)。当某个脚本或 CI 作业需要一个长时效、作用域狭窄的凭据时，再手动创建令牌。

## 令牌是权限上限，绝非权限来源

令牌永远只能收窄其所有者本已能做的事。一个只读令牌仍然只能读；一个作用域限定于某个智能体的令牌无法触及另一个。由此推出两条规则：

- **管理员操作需要登录。** 签发令牌、管理名册、转移或删除组织：这些操作需要你自己的登录会话，若以令牌呈现则会被拒绝，即便是管理员的写令牌也不行。令牌永远无法铸造另一个令牌，因此一个泄露的令牌无法孵化出一个常驻的立足点。
- **令牌不增添任何权限。** 把一个令牌交给某人，只授予他你访问权限中该令牌作用域所允许的那个子集，且仅在其存活期间有效。

## 创建令牌

在账户页面，创建一个令牌，包含：

- 一个**名称**，以便你日后能在列表中认出它，
- 一个**作用域**，读或写，
- 一个可选的**智能体**绑定，`owner/name`，用于把令牌限制在单个智能体上，以及
- 一个可选的**有效期**，以天为单位。

中枢只在创建时显示令牌字符串一次。它只存储一个 sha256 摘要，该摘要无法被还原回令牌，因此请现在就复制它。一个绑定到某个你只能读的智能体的写作用域令牌，会在创建时被拒绝，而不会先签发再在首次推送时失败。

与之等价的管理员 CLI 是 `agit-hub token add <name> [--user <owner>] [--agent <owner>/<name>] [--read|--write] [--ttl-days N]`，它同样只打印令牌一次。

## 使用令牌

把一个远端指向智能体的 `.git` URL，并将令牌放入 git 的密码字段（用户名可以是任意值）：

```bash
agit a remote add origin https://hub.example.com/alice/frontend.git
agit a push -u origin main
#   username: anything
#   password: the token
```

脚本也可以将其作为 bearer 令牌发送：

```bash
curl -H "Authorization: Bearer $AGIT_TOKEN" https://hub.example.com/api/agents
```

关于完整的客户端设置，参见 [将 CLI 连接到中枢](../integration/connect-cli-to-hub.md)。

## 列出与吊销

账户页面上的令牌列表会显示每个令牌的名称、作用域、智能体绑定、创建时间、有效期与最近使用时间，但绝不显示秘密本身。你看到的是你自己的令牌；站点管理员看到全部令牌。吊销一个令牌可立即将其停用。旧的无主令牌会被显示为不可用，而不会悄无声息地继续工作。

管理员 CLI 以 `agit-hub token list` 与 `agit-hub token rm <id>` 与之对应。

## 相关

- [账户](./accounts.md)：签发令牌所需要的登录会话。
- [签名密钥](./signing-keys.md)：登记一个可自动铸造短时效令牌的设备密钥。
- [认证](../integration/authentication.md)：推送、拉取、获取与克隆上的基于密钥的认证。
