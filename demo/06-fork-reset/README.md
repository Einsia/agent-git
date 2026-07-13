# Demo 06 — fork 试新方向 / reset 回到干净状态

对应 `使用场景.pdf` 场景 4 和场景 7。

## 这个 demo 回答什么问题

「我想让 agent 换个思路试试，但不想污染团队的共享 context。」
「有人合进来一批错的结论，agent 开始跑偏，怎么回到干净状态？」

## 这个 demo 最重要的一点

**agit 为这两个场景写的代码行数是 0。**

`branch` / `checkout` / `merge` / `reset` / `log` 全部原样转交 git。
这是「把 subject 当成文件路径」这个 reduction 的全部红利。

## 准备

```sh
./demo/06-fork-reset/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/06-fork-reset
```

你在 `payments` 分支上，它已经有一条结论。

---

## 场景 4：fork 出来试新方向

不在共享分支上直接改：

```console
$ agit branch fork-saga
$ agit checkout -q fork-saga
$ agit branch --show-current
```

让 agent 在这条 fork 上试：

```console
$ agit new refund/idea/saga -e human:alice@2026-07-09 -m '用 saga 模式重写退款，补偿事务替代两阶段提交。'
$ grep '^tier' ctx/refund/idea/saga.md
```

```text
tier: irreversible
```

`human:` 证据 = **人做出的决策**。它不随代码失效，只能被新的决策覆盖。

提交：

```console
$ agit add
$ agit commit -qm '试试 saga'
$ agit log
```

**方案跑通了 → 合回共享分支。**

```console
$ agit checkout -q payments
$ agit merge fork-saga --no-ff --no-edit
$ agit log
```

`--no-ff` 让这次合并在历史里留下一个 merge commit。不加的话 git 会 fast-forward，
历史里根本不会出现「那次合并」。

**没跑通的话，整条丢掉，共享分支毫发无损：**

```sh
git branch -D fork-saga
```

> 你们文档里吐槽 Claude Code 的 fork「非常难用，以及没办法 merge 回去」。
> 这里 merge 回去是免费的——因为它本来就是 git 的分支。

---

## 场景 7：reset

先记下合并之前的干净状态：

```console
$ GOOD=$(git rev-parse --short HEAD) ; echo "干净状态 = $GOOD"
```

模拟「有人从一条没验证过的分支合了一批鉴权结论进来」：

```console
$ agit checkout -q -b sketchy payments
$ agit new auth/session/ttl -e doc:docs/api-v1.md@2019-01-01 -m 'session 永不过期。'
$ agit add
$ agit commit -qm '鉴权结论（未验证）'
$ agit checkout -q payments
$ agit merge sketchy --no-ff --no-edit
$ agit log
```

**agent 开始跑偏。哪一条错了？**

```console
$ agit verify
```

```text
STALE        auth/session/ttl                       [STALE] doc:docs/api-v1.md@2019-01-01
                                                      ↳ docs/api-v1.md，2746 天前采集
```

`verify` 直接指认了是哪条结论有问题——它的依据是一份 2019 年的文档。

> 你们文档里说「Bob 从一个没验证过的分支 merge 了一批关于鉴权流程的结论，
> 里面有几条信息是错的，之后 Agent 给的建议就开始跑偏」。
>
> `verify` 让「哪几条是错的」从人工排查变成一条命令。

**回到合并之前：**

```console
$ agit reset --hard $GOOD
$ agit log
$ agit verify
```

```text
2 条 claim，证据全部新鲜。
```

丢掉的那次合并还在 reflog 里，随时能捞回来：

```console
$ git reflog | head -3
```

---

## 命令映射

| agit | 实际执行 |
|---|---|
| `agit branch` | `git branch` |
| `agit checkout` | `git checkout` |
| `agit merge` | `git merge`（我们的 driver 在 claim 冲突时被调用） |
| `agit reset` | `git reset` |
| `agit log` | `git log --oneline -- ctx` |

除了 `merge` 会在冲突时打印一段友好的摘要，其余全是原样透传。

## 接着看

[Demo 07](../07-secrets/) —— agent 读了 `.env`，push 出去怎么办？
