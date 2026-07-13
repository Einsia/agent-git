# Demo 02 — 一条 claim 是什么，为什么「没有出处的结论不入库」

## 这个 demo 回答什么问题

「context 里到底存的是什么？谁保证 agent 写的结论不是编的？」

## 准备

```sh
./demo/02-claim/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/02-claim
```

---

## 步骤 1 — 看一眼源代码

```console
$ sed -n '4p' models/user.ts
```

```text
  user_id: string;
```

---

## 步骤 2 — 写下第一条结论，并指明它的出处

```console
$ agit new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id，不是 uid。'
```

```text
新建 claim  ctx/api/user/id-field-name.md
  tier: reversible
  证据: file:models/user.ts:4 #a937b4a5
```

看它落盘成了什么：

```console
$ cat ctx/api/user/id-field-name.md
```

```text
---
subject: api/user/id-field-name
tier: reversible
author: alice
created: 2026-07-09T10:47:38Z
evidence:
- 'file:models/user.ts:4 #a937b4a5'
---

用户标识字段叫 user_id，不是 uid。
```

**两件事值得停下来看。**

**一、subject 就是文件路径。** `api/user/id-field-name` → `ctx/api/user/id-field-name.md`

这不是美学选择，是整个设计的支点。一个 claim 一个文件，于是 git 的三方树合并
**直接成为**语义合并（[Demo 04](../04-merge/) 会让你亲眼看到）。

**二、`#a937b4a5` 是采集当时那一行内容的 SHA-256 前 8 位。**

有了这个指针加摘要，才能回头问「源头还是当初那样吗」（[Demo 03](../03-stale/)）。
Shepherd / Zed / Claude Code 记录的都是**状态**，没有一个给结论附上指向源头的指针。

---

## 步骤 3 — 编造出处会怎样

指向一个不存在的行：

```console
$ agit new bogus/one -e file:models/user.ts:999 -m '瞎编的。'
```

```text
agit: 证据无法采集: file:models/user.ts:999: models/user.ts 只有 10 行，取不到 999-999
```

指向一个不存在的文件：

```console
$ agit new bogus/two -e file:nope.ts:1 -m '也是瞎编的。'
```

自己确认它**一个字节都没写**：

```console
$ ls ctx/bogus 2>/dev/null || echo '没有 ctx/bogus —— 被拒绝的 claim 不落盘'
```

`agit new` 在落盘**之前**回到源头把每条证据重读一遍。读不到就拒绝。

> **这一步是让 provenance 从「模型可以随便填的字段」变成硬约束的地方。**
>
> 将来接上 LLM 抽取时这道闸门还要再收紧一次：证据候选池先从 agent session 的
> `tool_use` / `tool_result` 里构造好——也就是「agent 实际看到过什么」——
> 模型只能从池子里选 locator。编造出处于是变成构造上不可能。

---

## 步骤 4 — 证据的类型决定 tier

命令输出作证据：

```console
$ agit new perf/orders/db-calls -e "cmd:grep -c 'await db' services/order.ts" -m 'OrderService 里有 2 处 await db 调用。'
$ grep '^tier' ctx/perf/orders/db-calls.md
```

```text
tier: compensable
```

人做的决策作证据：

```console
$ agit new refund/idea/saga -e human:alice@2026-07-09 -m '决定用 saga 模式重写退款。'
$ grep '^tier' ctx/refund/idea/saga.md
```

```text
tier: irreversible
```

| locator | 校验方式 | tier |
|---|---|---|
| `file:PATH:LINE[-LINE]` | 重读那几行，重算摘要 | `reversible` |
| `cmd:COMMAND` | 重跑命令（**默认不跑**，见 Demo 03） | `compensable` |
| `doc:REF@YYYY-MM-DD` | 采集超过 365 天判定陈旧 | `reversible` |
| `human:WHO@YYYY-MM-DD` | 不随代码失效 | `irreversible` |

一条 claim 有多条证据时，tier 取最强的（`irreversible` > `reversible` > `compensable`）。

三个 tier 借自 Shepherd 的 effect reversibility tier，但作用在**知识**上而非副作用上：

- 代码读出的事实，源变即失效
- 命令得出的结论，要重跑才知道
- 人做的决策，不能靠重放推翻，只能被新的决策覆盖

这个分层直接决定了 merge 冲突时的裁决优先级。

---

## 步骤 5 — `add` / `commit` 就是 git 的 `add` / `commit`

```console
$ agit status
$ agit add
$ agit status
$ agit commit -qm '三条结论'
$ agit log
```

`agit add` **只暂存 `ctx/`，绝不替你暂存代码。** `agit commit` 提交暂存区。

有暂存区是有意义的：将来从一个 session 抽出 20 条 claim，你会想先审一遍、挑几条提交——
那就是 `git add -p` 的场景，白拿。

---

## 自己再翻翻

```console
$ agit-state
$ cat ctx/refund/idea/saga.md
$ git log --oneline -- ctx
```

## 接着看

[Demo 03](../03-stale/) —— 三个月后代码变了，这条结论还能信吗？
