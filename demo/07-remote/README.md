# Demo 07 — 把 context 发给团队，同事一条命令复用

## 这个 demo 回答什么问题

「Alice 的 agent 花一下午查清楚的东西，怎么让同事直接接着用，而不用问她、翻聊天记录？」

对应 PRD 的 TeamExposure + ContextReuse。Agent Store 本身就是个 git 仓库，
所以 push/pull/clone 全部白拿。

## 准备

```sh
./demo/07-remote/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/07-remote
```

你是 alice，Agent Store 里有一条 context，远端已配好但还没推。

---

## 步骤 1 — Alice 发布 context

```console
$ agit -a log --oneline
$ agit -a push -u origin main
```

`agit -a push` 把 AgentState 推到团队远端。pre-push hook 先扫一遍密钥才放行
（Demo 06）。`agit -a <任意 git 命令>` 全部同构可用。

---

## 步骤 2 — 同事拉取（新工作区）

同事在自己机器上：先有代码仓库，`agit init` 建空的 Agent Store，指向同一个远端，拉：

```console
$ cd /tmp/agit-demo && cp -r 07-remote/models 07-remote/services /tmp/agit-demo/teammate-src 2>/dev/null; mkdir -p /tmp/agit-demo/teammate && cp -r 07-remote/models 07-remote/services 07-remote/docs /tmp/agit-demo/teammate/
$ cd /tmp/agit-demo/teammate && git init -q -b main . && git add -A && git -c user.email=t@x -c user.name=t commit -qm code
$ agit init >/dev/null && git -C .agit/agent remote add origin /tmp/agit-demo/07-agent-origin.git
$ agit -a pull origin main
$ agit -a log --oneline
```

同事的 Agent Store 现在带着 Alice 的结论。

---

## 步骤 3 — 不只是「Alice 说有 N+1」，而是带出处、且当场复验

```console
$ agit -a why latency/order-service/n-plus-1
```

```text
结论
  OrderService.list 有 N+1 查询，某次改动引进来的。
出处链
  [FRESH] file:services/order.ts:7-10 #...
        services/order.ts:7 → // 每个订单一次查询 —— N+1。...
```

同事拿到的是「Alice 说有 N+1，依据是这几行代码，而且我这边刚重新验证过，还是当初那样」。

```console
$ agit -a verify
```

> `agit init` 之后同事的 Agent Store 才装上 merge driver 与 hook——
> `.gitattributes` 跟着 clone 走，但驱动配置不会（git 的安全设计）。

## 存储回顾

| | Environment | Agent Store |
|---|---|---|
| 是什么 | 代码仓库 | `.agit/agent`，独立 git 仓库 |
| 远端 | 你的代码远端 | 团队 Agent Store 远端 |
| 发布 | `git push` | `agit -a push` |

## 全部 demo

01 两个库 · 02 抽取 · 03 fact+过期 · 04 合并 · 05 配对 · 06 密钥 · **07 远端**
