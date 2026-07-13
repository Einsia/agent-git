# Demo 05 — claim 级的语义 diff

## 这个 demo 回答什么问题

「review 一个 context 变更时，我怎么知道哪条结论**真的变了**，哪条只是**补了个证据**？」

## 准备

```sh
./demo/05-diff/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/05-diff
```

setup 已经建好两条 claim 并提交。

---

## 步骤 1 — 制造三种性质完全不同的变化

**① 新增一条结论**

```console
$ agit new auth/token/expiry -e file:services/auth.ts:1 -m 'token 有效期 30 分钟。'
```

**② 改一条结论的正文** —— 这是真正的知识变更

```console
$ sed -i 's/^退款穿过三个服务.*$/退款穿过四个服务：新增了风控前置检查。/' ctx/refund/flow/services.md
```

**③ 只给另一条追加一条证据，正文一个字不动**

```console
$ sed -i "s|^- 'file:models/user.ts:4.*|&\n- human:alice@2026-07-09|" ctx/api/user/id-field-name.md
$ agit add
```

---

## 步骤 2 — git 看到的

```console
$ git diff --cached --stat -- ctx
```

```text
 ctx/api/user/id-field-name.md |  1 +
 ctx/auth/token/expiry.md      | 10 ++++++++++
 ctx/refund/flow/services.md   |  2 +-
 3 files changed, 12 insertions(+), 1 deletion(-)
```

三个文件，若干行。**它不知道哪条是知识变更，哪条只是证据刷新。**

---

## 步骤 3 — agit 看到的

```console
$ agit diff
```

```text
+ 新增结论   auth/token/expiry
· 仅证据刷新 api/user/id-field-name   （结论未变，不构成冲突）
~ 结论变更   refund/flow/services
    旧: 退款穿过三个服务：PaymentGateway → LedgerService → NotifyService。
    新: 退款穿过四个服务：新增了风控前置检查。
```

| 记号 | 含义 | 判定 |
|:--:|---|---|
| `+` | 新增结论 | git status = A |
| `-` | 删除结论 | git status = D |
| `~` | **结论变更** | 正文变了 |
| `·` | 仅证据刷新 | 正文没变，evidence 变了 |

---

## 为什么「仅证据刷新」这一类很重要

它直接对应 merge driver 的第一条分支：**正文相同 → 证据取并集，不算冲突。**

现实场景：Alice 的 agent 从代码里确认了字段名；Bob 的 agent 从测试里也确认了同一件事。
两人得出**同样的结论**，但**不同的证据**。

这不该是冲突——应该合并成一条结论、两份证据。

如果没有 claim 级的 diff，你在 review 时看到的只是「`evidence:` 那一行变了」，
无从判断这是补证据还是改结论。

---

## 步骤 4 — 提交

```console
$ agit commit -qm '三种变化'
$ agit diff
```

```text
ctx/ 相对 HEAD 没有变化。
```

---

## 步骤 5 — 删除一条结论

```console
$ git rm -q ctx/auth/token/expiry.md
$ agit diff
```

```text
- 删除结论   auth/token/expiry
```

> 顺带一个 git 的细节：刚才那条 claim 如果只在暂存区、从没进过 HEAD，
> `git rm` 会拒绝（"has changes staged in the index"）。所以要先 commit 再删。

---

## 步骤 6 — 和任意提交比较

```console
$ git commit -qm '删掉一条'
$ agit diff HEAD~2
```

同时看到三种变化叠加的结果。

---

## 实现

不是自己写 diff 算法。

`git diff --name-status <base> -- ctx` 拿到变更文件列表，对每个 `M` 的文件
用 `git show <base>:<path>` 取旧版本，两边都 parse 成 `Claim`，比较 `body` 和 `evidence`。

## 接着看

[Demo 06](../06-fork-reset/) —— fork 试新方向、从坏合并里回来。agit 写了 0 行代码。
