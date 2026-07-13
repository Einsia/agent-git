# Demo 03 — fact 带证据，证据会过期

## 这个 demo 回答什么问题

「AgentState 里的『事实』凭什么可信？三个月后代码变了，它还成立吗？」

## 准备

```sh
./demo/03-facts/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/03-facts
```

---

## 步骤 1 — 手工写一条带证据的 fact

```console
$ agit -a new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id，不是 uid。'
```

```text
新建 fact  state/facts/api/user/id-field-name.md
  tier: reversible
  证据: file:models/user.ts:4 #a937b4a5
```

**关键的双根**：fact 文件住在 **Agent Store**，但证据 `file:models/user.ts:4` 指向
**代码仓库**。`#a937b4a5` 是采集当时那一行的摘要。

没有可验证证据的结论**不入库**：

```console
$ agit -a new bogus/x -e file:models/user.ts:999 -m '瞎编的。'
```

（第 999 行不存在，被拒绝，一个字节都不写。）

---

## 步骤 2 — 校验：证据还对得上吗

```console
$ agit -a verify
```

```text
FRESH        api/user/id-field-name               [FRESH] file:models/user.ts:4 #a937b4a5
                                                    ↳ models/user.ts:4 → user_id: string;
1 条 fact，证据全部新鲜。
```

`agit -a verify` 从代码仓库把每条证据重读一遍、重算摘要、比对。

---

## 步骤 3 — 有人改了代码，但没人改 context

```console
$ sed -i 's/  user_id: string;/  userId: string;/' models/user.ts
$ git commit -qam '重命名 user_id -> userId'
$ agit -a verify
```

```text
STALE        api/user/id-field-name               [STALE] file:models/user.ts:4 #a937b4a5
                                                    ↳ models/user.ts:4 已变更（a937b4a5 → 06208f05）
1 / 1 条 fact 的证据已失效或不可达。
```

**agit 自己发现了。** 退出码非零——可以挂 CI。

> 三个竞品（Shepherd / Zed checkpoint / Claude Code rewind）原理上做不到这件事：
> 它们记录的是「状态」，没有一个给结论附上指向源头的指针。

---

## 步骤 4 — 出处链

```console
$ agit -a why api/user/id-field-name
```

结论 + 每条证据的当前状态 + Agent Store 里的提交历史。

---

## 步骤 5 — schema 校验

```console
$ agit -a validate
```

`agit/v1-draft` 校验：每条 fact 能解析、有证据、无密钥。对象缺失/格式错会显式报出来。

## 接着看

[Demo 04](../04-merge/) — 两个人的 context 合并，冲突用证据裁决。
