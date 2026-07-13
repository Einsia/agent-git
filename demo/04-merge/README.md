# Demo 04 — 合并：真冲突浮出来，假冲突根本不存在

## 这个 demo 回答什么问题

「两个 agent 得出矛盾的结论，怎么办？」

以及一个更容易被忽略、但更重要的问题：

**「两个 agent 学到不相干的知识，会不会互相打架？」**

对应 `使用场景.pdf` 场景 5。**这是整个项目的核心。**

## 准备

```sh
./demo/04-merge/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/04-merge
```

setup 建了四条分支（它会打印一张表）：

- **alice** 读代码 → 字段叫 `user_id`（证据：`models/user.ts:4`）；另外独立查清一个延迟问题
- **bob** 读 2024 年的老文档 → 字段叫 `uid`（证据：`docs/api-v1.md@2024-03-11`）；另外独立查清退款流程
- **carol / dave** 一对证据强度完全相同的矛盾结论

你现在在 `alice` 上。

---

## 步骤 1 — 看清起点

```console
$ git log --oneline --graph --all
$ find ctx -name '*.md' | sort
```

alice 上只有两条 claim。bob 那两条她还没有。

---

## 步骤 2 — 合并 bob 的 context

```console
$ agit merge bob
```

```text
Auto-merging ctx/api/user/id-field-name.md
CONFLICT (add/add): Merge conflict in ctx/api/user/id-field-name.md
Automatic merge failed; fix conflicts and then commit the result.

1 条 claim 冲突：
  api/user/id-field-name

每个冲突文件末尾都附了双方证据的当场校验结果与建议。
看一眼，然后： agit resolve <subject> --take ours|theirs
```

**只有一条冲突。就是那条真的。**

现在看看另外三条 claim 怎么样了：

```console
$ git status --short
$ find ctx -name '*.md' | sort
```

```text
AA ctx/api/user/id-field-name.md      ← 冲突（AA = 两边都新建了这个文件）
A  ctx/refund/flow/services.md        ← bob 的，自动合进来了

ctx/api/user/id-field-name.md
ctx/latency/order-service/n-plus-1.md   ← alice 的，还在
ctx/refund/flow/services.md             ← bob 的，进来了
```

Alice 的 latency 结论和 Bob 的 refund 结论**都在，一条都没丢**，
而且 git 一句提示都没给——因为它们根本不相干。

---

## 这一步是整个设计的核心，值得停一下

如果 context 存成**一个** `ctx.md` 文件会怎样？

Alice 和 Bob 各自把新学到的知识追加在文件末尾。两段追加**在同一个位置**，
于是 git 报第二个冲突——**一个彻头彻尾的假冲突**。

而真冲突和假冲突长得一模一样，reviewer 分不出来。最要命的是：
随手选一边 `HEAD`，**另一个 agent 的知识就被静默删除了，一句提示都没有。**

> 「合并时静默丢失知识」是这个产品唯一不能犯的错误。

把 subject 当成文件路径、一个 claim 一个文件，这个错就在**结构上不可能发生**。

副产品：`branch` / `fork` / `reset` / `log` / `push` / `pull` / `clone` 全部白拿，
一行代码不用写（[Demo 06](../06-fork-reset/)、[Demo 08](../08-remote/)）。

---

## 步骤 3 — 冲突文件里有什么

```console
$ cat ctx/api/user/id-field-name.md
```

```text
<<<<<<< ours
---
subject: api/user/id-field-name
evidence:
- 'file:models/user.ts:4 #a937b4a5'
---

用户标识字段叫 user_id。
=======
---
subject: api/user/id-field-name
evidence:
- 'doc:docs/api-v1.md@2024-03-11'
---

用户标识字段叫 uid。
>>>>>>> theirs

# ─────────────── agit 证据裁决 ───────────────
# ours   (tier=reversible)
#     [FRESH] file:models/user.ts:4 #a937b4a5 — models/user.ts:4 → user_id: string;
# theirs (tier=reversible)
#     [STALE] doc:docs/api-v1.md@2024-03-11 — docs/api-v1.md，850 天前采集
#
# 建议采纳: ours   （ours=FRESH / theirs=STALE，依据合并时的证据状态，非模型判断）
#   agit resolve api/user/id-field-name --take ours
```

**merge driver 在合并那一刻做了两件事：**

1. 重新打开 `models/user.ts`，读第 4 行，重算摘要 → 和 ours 记录的一致 → `FRESH`
2. 看 theirs 的 `doc:` 采集日期 → 850 天前 → `STALE`

然后按 `证据状态 → tier` 排序给建议。

> **同样的三份输入，跑一万次结果一样。模型不进裁决路径。**
> 否则这不是版本控制，是掷骰子。

---

## 步骤 4 — 裁决

```console
$ agit resolve api/user/id-field-name --take ours
$ cat ctx/api/user/id-field-name.md
```

冲突标记和 `# agit` 注释全被剥掉，落盘的是一条规范化的 claim。

```console
$ git commit -qm 'merge bob：采纳 user_id'
$ agit verify
$ agit log
```

---

## 步骤 5 — 证据强度相同时，它拒绝猜

carol 和 dave 各自有一条 `api/user/role-field`，证据都是**新鲜的 doc**，tier 相同。

```console
$ git checkout -q carol
$ agit merge dave
$ tail -8 ctx/api/user/role-field.md
```

```text
# ─────────────── agit 证据裁决 ───────────────
# ours   (tier=reversible)
#     [FRESH] doc:docs/api-v1.md@2026-07-01
# theirs (tier=reversible)
#     [FRESH] doc:docs/api-v1.md@2026-07-02
#
# 无法自动判定：双方证据强度相同。需要人类裁决。
#   agit resolve api/user/role-field --take ours|theirs
```

**它不猜。** 把两条都带着各自的证据摆给你看，交给你判断。
这正是你们文档里写的：「把两条都列出来，带着各自的证据，交给 reviewer 判断」。

---

## merge driver 的五条分支

git 只在「同一路径两边都改了」时调用它，传入 `%O`(祖先) `%A`(我方，也是输出) `%B`(对方) `%P`(路径)。

| 情况 | 行为 | 退出码 |
|---|---|:--:|
| 正文相同 | **证据取并集**，不算冲突 | 0 |
| 我方未动 | 取对方 | 0 |
| 对方未动 | 保留我方 | 0 |
| 双方都改 | 当场重新校验双方证据 → 生成带建议的冲突 | 1 |
| 解析不了 | 退回原始三方冲突，**绝不猜** | 1 |

每条都有 golden test（`tests/cli.rs`），包括 `merge_recommendation_is_symmetric`——
把双方对调，建议必须翻转。**确定性不等于偏心。**

## 接着看

[Demo 05](../05-diff/) —— 哪条结论真的变了，哪条只是补了证据？
