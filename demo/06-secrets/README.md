# Demo 06 — 密钥不得进入 context

## 这个 demo 回答什么问题

「agent 查数据库问题时 `cat` 了 `.env`，密码进了 context。一旦 push、同事 pull，密码就发到
每个人机器上。怎么挡？」

PRD 硬性要求：**secret 不得进入 Hub。**

## 准备

```sh
./demo/06-secrets/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/06-secrets
```

---

## 第一道：采集时拒绝

```console
$ agit -a new db/conn -e file:.env:1 -m '连接串在这。'
```

```text
agit: 证据无法采集: file:.env:1: .env 在 denylist 上（.env / 私钥 / 被 gitignore 的路径）。
```

`.env`、`*.pem`、`id_rsa`、以及任何被 `.gitignore` 命中的路径，只记 locator、绝不抄内容。

```console
$ agit -a new tls/key -e file:server.pem:1 -m '证书私钥。'
```

---

## 第二道：落盘前扫正文

```console
$ agit -a new db/note -e file:models/user.ts:1 -m 'DATABASE_URL=postgresql://payments:hunter2ButLonger@db.internal:5432/payments'
```

```text
拒绝写入：fact 里含有疑似密钥。
```

扫描同时覆盖正文**和证据快照**——证据会把源文件内容抄进来，容易漏。

---

## 第三道：commit hook

就算有人绕过 `agit -a new` 直接写文件，Agent Store 的 pre-commit hook 还会拦：

```console
$ mkdir -p .agit/agent/state/facts/db
$ printf -- '---\nsubject: db/creds\ntier: reversible\nauthor: x\ncreated: 2026-07-13T00:00:00Z\nevidence:\n- '"'"'file:models/user.ts:1'"'"'\n---\n\nAKIAIOSFODNN7EXAMPLE\n' > .agit/agent/state/facts/db/creds.md
$ agit -a add -A
$ agit -a commit -m "试图提交密钥" ; echo "退出码=$?"
```

```text
发现疑似密钥：
  state/facts/db/creds.md:... [aws-access-key-id]
退出码=1
```

提交被拦下。清掉：

```console
$ rm .agit/agent/state/facts/db/creds.md
$ agit -a scan
```

> Shepherd / Zed / Claude Code 都没有这道防线——不是疏忽，是它们不分享 context。
> `agit -a validate` 也会在校验时扫一遍（Demo 03）。

## 接着看

[Demo 07](../07-remote/) — 把 context push 给团队，同事一条命令复用。
