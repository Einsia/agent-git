# Demo 07 — 密钥的三道防线

对应 `使用场景.pdf` 场景 8。原文那里打了个问号：「commit hook?」——答案是：hook，加另外两道。

## 这个 demo 回答什么问题

> 「小林查一个连数据库的问题时，让 Agent `cat` 了一下 `.env`，又跑了条命令把 connection string
> 打了出来。这段东西现在就躺在他 Agent 的 context 里，一旦他 push 这条分支、同事 clone 下来，
> 这个密码就跟着 context 发到每个人机器上。」

## 准备

```sh
./demo/07-secrets/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/07-secrets
```

setup 会建一个**真的裸远端**。步骤 3 里你会**真的尝试 push**，看到 git 拒绝。

---

## 步骤 0 — 看看 .env 里有什么

```console
$ cat .env
```

```text
DATABASE_URL=postgresql://payments:hunter2ButLonger@db.internal:5432/payments
AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
STRIPE_SECRET=sk_live_...
```

---

## 第一道：采集时拒绝，内容根本不进 context

```console
$ agit new db/conn/password -e file:.env:1 -m '数据库连接串在这里。'
```

```text
agit: 证据无法采集: file:.env:1: .env 在 denylist 上（.env / 私钥 / 被 gitignore 的路径）。
agit 拒绝把它的内容抄进 context —— 这正是密钥泄漏的路径。
```

私钥文件同样：

```console
$ agit new tls/key -e file:server.pem:1 -m '证书私钥。'
```

denylist 覆盖：

- 文件名：`.env`、`.env.*`、`.envrc`、`.netrc`、`.npmrc`、`credentials`、
  `id_rsa` / `id_dsa` / `id_ecdsa` / `id_ed25519`
- 扩展名：`.pem` `.key` `.p12` `.pfx` `.jks` `.keystore`
- **任何被 `.gitignore` 命中的路径**（走 `git check-ignore`）

对这些路径 agit 只记 locator 和摘要，**绝不把内容抄进 context**。

---

## 第二道：`agit new` 落盘前扫描

就算把密钥硬塞进正文：

```console
$ agit new db/conn/note -e file:models/user.ts:1 -m 'DATABASE_URL=postgresql://payments:hunter2ButLonger@db.internal:5432/payments'
```

```text
拒绝写入：claim 里含有疑似密钥。
  行 10  [connection-string]  post…******
```

别的形态也认：

```console
$ agit new aws/key -e file:models/user.ts:1 -m 'AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE'
```

| 规则 | 抓什么 |
|---|---|
| `aws-access-key-id` | `AKIA[0-9A-Z]{16}` |
| `github-pat` | `ghp_…` / `github_pat_…` |
| `slack-token` | `xox[baprs]-…` |
| `openai-key` / `anthropic-key` | `sk-…` / `sk-ant-…` |
| `private-key-block` | `-----BEGIN … PRIVATE KEY-----` |
| `jwt` | `eyJ….….…` |
| `assigned-secret` | `password/secret/token/api_key` 后面跟 `=` 或 `:` |
| `connection-string` | `postgres://user:pass@host` 一类 |
| `high-entropy-string` | 32 字符以上、Shannon 熵 > 4.2 |

> **扫描必须同时覆盖 claim 正文和证据快照。**
> 证据会把源文件内容抄进来——这一点很容易漏。

---

## 第三道：pre-commit 与 pre-push hook

现在模拟一个**不走 `agit new`、直接写文件**的流氓 agent。
`rogue-claim.md` 已经放在仓库根目录了：

```console
$ mkdir -p ctx/db
$ cp rogue-claim.md ctx/db/creds.md
$ git add ctx/db/creds.md
```

先试试正常提交 —— **pre-commit hook 会拦**：

```console
$ git commit -qm '正常提交' ; echo "退出码 = $?"
```

```text
发现疑似密钥：
  ctx/db/creds.md:10  [connection-string]  post…******
退出码 = 1
```

他用 `--no-verify` 绕过去了：

```console
$ git commit --no-verify -qm '绕过 pre-commit'
$ git log --oneline -1
```

**push 那一刻，还有最后一道：**

```console
$ agit push -u origin main ; echo "退出码 = $?"
```

```text
发现疑似密钥：
  ctx/db/creds.md:10  [connection-string]  post…******

1 处。context 一旦 push，同事 clone 下来就带着它们。
修掉它。或者用 --no-verify 绕过这道 hook，显式承担后果。
error: failed to push some refs to '/tmp/agit-demo/07-secrets-origin.git'
退出码 = 1
```

**密钥没有离开这台机器。**

> `pre-push` 不能省。分支可能是从别处 merge 来的——密钥不一定是你自己 commit 的。

---

## 清理掉，push 就通了

```console
$ git reset --hard HEAD~1
$ agit scan ; echo "退出码 = $?"
$ agit push -u origin main
```

---

## hook 装在哪

```console
$ cat .git/hooks/pre-commit
$ cat .git/hooks/pre-push
```

**hook 不进仓库、不跟着 clone。** 新 clone 的仓库必须重跑 `agit init` 才有这两道
（同 [Demo 01](../01-init/) 里 merge driver 的道理）。

---

## 为什么竞品没有这个

Shepherd、Zed 的 checkpoint、Claude Code 的 `/rewind`，都没有密钥扫描。

**不是疏忽。是它们不分享 context。** 一个只能 undo、不能 push 的东西，
不需要担心密钥跟着 context 发到每个人机器上。

反过来说，这道防线的存在本身就说明我们在做一件它们没在做的事。
它也重构了叙事——agit 不只是效率工具，是 **context 共享的安全层**。

## 接着看

[Demo 08](../08-remote/) —— 跨人、跨时区、新人上手。
