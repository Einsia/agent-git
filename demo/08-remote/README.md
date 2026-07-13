# Demo 08 — push / pull / clone：跨人、跨机器

对应 `使用场景.pdf` 场景 1（新人 clone）、场景 2（本地多 agent 同步）、场景 3（跨时区接力）。

## 这个 demo 回答什么问题

「Alice 的 agent 花一下午查清楚的东西，怎么让北京的小林第二天早上直接接着做？」
「新人入职，怎么一条命令拿到团队攒了两年的 context？」

## 这三个场景零竞争

Shepherd、Zed 的 checkpoint、Claude Code 的 `/rewind` **全部是纯本地的**。

- Shepherd 全文 `push` 出现 6 次，没有一次是把 trace 推到远端；`pull` 出现 **0** 次；
  `multi-user` 出现 **0** 次
- Zed 的 checkpoint 是 thread-scoped 的本地 undo
- Claude Code 的 checkpoint 存在 `~/.claude/file-history/<session-uuid>/`，30 天后清理，
  没有导出、没有分享

而在 agit 里，这三个场景**一行代码都没写**——`push` / `pull` / `clone` / `checkout` / `branch`
全部原样转交 git。

## 准备

```sh
./demo/08-remote/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/08-remote
```

你是 Alice，在 `alice` 分支上，两条结论还没 push。

---

## 场景 3：跨时区接力

### 旧金山，Alice 下班前

```console
$ agit log
$ agit push -u origin alice
```

```text
 * [new branch]      alice -> alice
Branch 'alice' set up to track remote branch 'alice' from 'origin'.
```

注意 `pre-push` hook 先跑了一遍密钥扫描才放行（[Demo 07](../07-secrets/)）。

### 北京，第二天早上，小林

```console
$ git clone -q /tmp/agit-demo/08-origin.git /tmp/agit-demo/08-lin
$ cd /tmp/agit-demo/08-lin
$ git config user.name lin ; git config user.email lin@payments.io
```

**clone 之后第一件事**——先感受一下不装驱动会怎样：

```console
$ agit verify
```

```text
warning: 本仓库尚未安装 agit 的 merge driver。
         `.gitattributes` 会跟着 clone 走，但驱动配置不会（git 的安全设计）。
         跑一次 `agit init` 修复。
```

装好：

```console
$ agit init
```

拉 Alice 的 context：

```console
$ agit pull origin alice
$ agit log
$ find ctx -name '*.md' | sort
```

**看一条结论的出处链：**

```console
$ agit why latency/order-service/n-plus-1
```

```text
结论
  OrderService.list 有 N+1 查询，某次改动引进来的。

出处链
  [FRESH] file:services/order.ts:7-10 #a9d0e23b
        services/order.ts:7 → // 每个订单一次查询 —— N+1。
```

小林的 agent 拿到的不是「Alice 说有 N+1」，而是
**「Alice 说有 N+1，依据是这几行代码，而且我刚刚重新验证过，它们还是当初那样」**。

### 小林在她的基础上继续，推回去

```console
$ agit new refund/flow/services -e file:services/refund.ts:8-10 -m '退款穿过三个服务。'
$ agit add
$ agit commit -qm '查清退款流程'
$ agit push -q origin HEAD:alice
$ agit log
```

---

## 场景 1：新人 clone 整个团队的 context

```console
$ git clone -q /tmp/agit-demo/08-origin.git /tmp/agit-demo/08-newbie
$ cd /tmp/agit-demo/08-newbie
$ git config user.name newbie ; git config user.email newbie@payments.io
$ agit checkout -q alice
$ agit init
$ agit log
$ find ctx -name '*.md' | sort
$ agit verify
```

一条命令，拿到团队攒下的全部结论。**每一条都能追到出处，而且当场重新验证过。**

> 老办法：翻代码、翻 wiki、挨个问人，一周。

---

## 场景 2：本地开新 session 前先 checkout

context 跟着分支走。

```console
$ agit checkout -q main
$ find ctx -name '*.md' | sort
$ agit checkout -q alice
$ find ctx -name '*.md' | sort
```

`main` 上什么都没有；`alice` 上有三条。开新 session 之前 `agit checkout <branch>`，
agent 就带着上次的理解。

> 你们文档里说 `--resume`「只能顺着那一条 session 接下去」。
> `checkout` 可以任意跳，可以 fork，可以 merge。

---

## 必须记住：clone 之后要 `agit init`

`.gitattributes` 跟着 clone 走，但 `merge.agit.driver` 和 hooks **不会**。
这是 git 有意的安全设计（仓库不能让 clone 它的人执行任意命令）。

不装的后果：`.gitattributes` 说「用 agit 合并 `ctx/**`」，git 找不到驱动，
**退化成按行合并**——一堆原始 frontmatter 的冲突，而且没有证据裁决。

将来 `agit clone` 应该做成一个 wrapper，clone 完顺手装好。现在还没做。

---

## v0 的边界

`ctx/` 放在代码仓库里，所以 **context 跟着代码分支走**，`agit push` 推的是整条分支。

- **好处**：clone / push / pull / fork / reset 全部免费
- **代价**：**agent 绑死在 project 上**，做不了场景 6（前端和后端共享同一个 `api-agent`）

那需要独立的 agent 仓库和 manifest，是 v2 的数据模型。不在演示里塞半成品。

## 跑完之后

```sh
ls /tmp/agit-demo/          # 四个仓库都在：08-origin.git / 08-remote / 08-lin / 08-newbie
```
