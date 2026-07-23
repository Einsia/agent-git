---
sidebar_position: 1
title: 将 CLI 连接到中枢
---

# 将 CLI 连接到中枢

中枢（Hub）是团队自行运行的一台服务器，用于托管智能体（agent）并在 Web UI 中浏览它们。你通过普通的客户端命令来访
问它：不存在任何中枢专属的命令动词。本页介绍如何将一台机器一次性连接到中枢，从而使 `agit a push`、`agit a pull`
和 `agit a clone` 能够自行完成认证。

存在两条凭据路径。注册一次签名密钥，之后每一次 push 和 pull 都无需复制令牌即可认证。在未注册密钥的场景下，或在脚本
中，令牌是回退方案。优先选择密钥路径。

## 密钥路径：注册一次，永久 push

在你用于 push 的每一台机器上，将该机器的公钥注册到你的中枢账户。

1. 打印本机的注册（enroll）密钥块：

   ```bash
   agit identity register you
   ```

   将 `you` 替换为你的中枢用户名。该命令离线运行：它派生本机的公钥、自签名一个注册密钥块并打印出来。没有任何内容
   离开本机。你的私钥绝不会离开 `$AGIT_HOME/identity`。

2. 复制打印出的密钥块。在中枢的 Web UI 中，打开 Account，然后进入 Signing keys，将其粘贴进去。中枢会验证自签名
   并将该密钥注册到你的账户名下。

3. 确认本机的状态：

   ```bash
   agit identity show
   ```

密钥注册完成后，`agit a push`（以及 `pull`、`fetch`、`clone`）便通过用已注册密钥签署服务端挑战、并将其交换为一枚
短时效令牌来完成认证。你在每次 push 时都无需粘贴任何内容。agit 仅为你的中枢主机充当 git 凭据助手；github、gitlab
等其他远程绝不会被触及。关于完整模型参见[认证](./authentication.md)，关于管理已注册密钥参见
[签名密钥](../hub/signing-keys.md)。

:::note
在你用于 push 的每一台机器上都要注册。密钥是按机器区分的，因此一台笔记本电脑和一个 CI 运行器各自注册各自的密钥。你
可以在 Web UI 中吊销其中一个而不影响其他机器。
:::

## 为中枢命名

基于密钥的认证仅对 agit 已知晓为中枢的主机触发：`AGIT_HUB_URL` 的主机，或当前活动智能体所绑定的存储库（store）远
程的主机。一旦你将某个智能体 push 到中枢，其绑定的远程就会使该主机变为已知。在首次 push 之前，请设置 `AGIT_HUB_URL`：

```bash
export AGIT_HUB_URL=https://hub.example.com
```

对某个中枢存储库执行 `git clone` 时，只有当该 URL 的主机已被本机声明为中枢，才会自动签发令牌。任意 URL 绝不会触发
签名挑战。这正是 clone 路径要求主机必须先为已知的原因。

## 令牌路径：回退方案

在未注册密钥的场景下，或在不应携带个人密钥的脚本中，使用令牌。

1. 在中枢的 Web UI 中创建一枚令牌。将其范围限定到单个智能体、设定时限，并可随时吊销。参见[令牌](../hub/tokens.md)。

2. 通过以下三种方式之一提供它：

   - 当 push 或 pull 请求时，将其输入到 git 的密码提示中。你的用户名即账户名。
   - 将其放入存储库远程 URL 的密码字段。
   - 导出为环境变量：

     ```bash
     export AGIT_HUB_URL=https://hub.example.com
     export AGIT_HUB_TOKEN=<token>
     export AGIT_HUB_USER=you
     ```

`AGIT_HUB_TOKEN` 会覆盖从远程 URL 中解析出的任何凭据。令牌是权限的上限，而非权限的来源：只读令牌依然只能读取，而
Web UI 中的管理操作需要登录，绝不接受令牌。

## 验证连接

```bash
agit a push
```

如果中枢拒绝该 push，它会指出其认证所用的账户以及缺失的权限，因此令牌错误、缺少授权和只读范围三者很容易区分开来。若
要首次发布一个智能体并记录其来源，参见[发布与获取](./sharing.md)。
