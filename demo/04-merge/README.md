# Demo 04 — 合并两个人的 context：真冲突浮出来，假冲突不存在

## 这个 demo 回答什么问题

「两个 agent 得出矛盾的结论怎么办？两个 agent 学到不相干的知识会不会互相打架？」

对应 PRD 的团队协作：`agit -a merge`。

## 准备

```sh
./demo/04-merge/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/04-merge
```

Agent Store 里已建好 alice / bob 两条分支（见 setup 打印的表）。你在 alice。

---

## 步骤 1 — 合并 bob 的 context

```console
$ agit -a merge bob
```

```text
CONFLICT (add/add): Merge conflict in state/facts/api/user/id-field-name.md
```

**只有一条冲突，就是那条真的**（`user_id` vs `uid`）。

```console
$ agit -a status --short
$ find .agit/agent/state/facts -name '*.md' | sort
```

bob 的 `refund/flow/services` 和 alice 的字段结论**不相干，被静默、正确地合并了**——
一个 fact 一个文件，subject 即路径，git 的三方合并因此天然区分「同一结论」和「不同知识」。

---

## 步骤 2 — 冲突里带着证据裁决

```console
$ cat .agit/agent/state/facts/api/user/id-field-name.md
```

```text
<<<<<<< ours
用户标识字段叫 user_id。
=======
用户标识字段叫 uid。
>>>>>>> theirs

# ─────────────── agit 证据裁决 ───────────────
# ours   (tier=reversible)
#     [FRESH] file:models/user.ts:4 #a937b4a5 — models/user.ts:4 → user_id: string;
# theirs (tier=reversible)
#     [STALE] doc:docs/api-v1.md@2024-03-11 — docs/api-v1.md，... 天前采集
#
# 建议采纳: ours   （ours=FRESH / theirs=STALE，依据合并时的证据状态，非模型判断）
#   agit -a resolve api/user/id-field-name --take ours
```

**merge driver 在 Agent Store 里运行，却把 `file:` 证据解析到代码仓库**，当场重读
`models/user.ts:4` → FRESH；bob 的 2024 文档 → STALE。裁决是确定性的，模型不进裁决路径。

---

## 步骤 3 — 采纳

```console
$ agit -a resolve api/user/id-field-name --take ours
$ agit -a add -A
$ agit -a commit -m "merge bob：采纳 user_id"
$ agit -a verify
```

冲突标记与裁决注释都被剥掉，落盘的是一条规范化 fact。

## 接着看

[Demo 05](../05-workspace/) — context 的版本钉在它所基于的代码版本上。
