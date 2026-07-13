# Demo 02 — 从 session 抽取 AgentState

## 这个 demo 回答什么问题

「PRD 的头号功能 ContextManagement：怎么把一次 agent session 里『读过什么、得出什么结论』
提炼成紧凑、可 diff 的 AgentState？」

## 准备

```sh
./demo/02-import/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/02-import
```

setup 在仓库根放了一份 `session.jsonl`（模拟 Claude Code 的一次会话：读了
`models/user.ts` 和 `services/refund.ts`，跑了一条 grep）。

---

## 步骤 1 — 看有哪些 adapter

```console
$ agit adapter
```

```text
  claude-code    Claude Code —— 解析 ~/.claude/projects/<slug>/<session>.jsonl（已实现）
  codex          Codex —— 接口已预留，export/import/validate 待实现（桩）
```

Codex 是留好的接口桩——拿到样本就能填，上层不改。

---

## 步骤 2 — 确定性抽取

```console
$ agit -a import session.jsonl
```

```text
从 claude-code session 抽取 AgentState：
  目标      : 1 条 prompt
  证据池    : 2 条 file 证据已对齐基线，0 条跳过
              1 条命令（只记不跑）
  artifact  : 1 个
```

这一步**不调模型**，全是确定性解析：

- **目标** 来自用户 prompt
- **证据候选池** 来自 agent 的 Read（`file:` 证据）和 Bash（`cmd:` 证据）
- `file:` 证据**当场对齐当前代码基线**重算摘要
- `cmd:` 证据**只记不跑**——session 里的命令可能有副作用，import 绝不执行它们
- **artifact** 来自 Write/Edit

看抽出来的东西：

```console
$ cat .agit/agent/state/goals.md
$ cat .agit/agent/state/_evidence_pool.md
```

`_evidence_pool.md` 是「结论」的**原材料**，不是结论本身。

---

## 步骤 3 — 语义归纳成 fact（可选，调本机 claude）

证据池 → 带出处的结论，这一步需要模型。用本机 `claude`：

```sh
agit -a import --summarize session.jsonl
```

它把证据池喂给本机 claude，让它归纳出 fact。**安全约束是全部要点**：模型只能引用
证据池里 agent 真读过的文件、真跑过的命令——**编造出处在构造上不可能**。归纳出的 fact
形如：

```markdown
---
subject: api/user/id-field-name
evidence:
- 'file:models/user.ts:4 #a937b4a5'    # 只来自证据池，带对齐基线的摘要
---
用户标识字段叫 user_id。
```

> 这一步默认不跑（依赖本机 claude 登录）。不想用模型？下一课用 `agit -a new` 手工提炼。

---

## 步骤 4 — 提交这份 context

```console
$ agit -a add -A
$ agit -a commit -m "导入会话 context"
$ agit -a log --oneline
```

溯源信息（`_session.json`）记下了这份 AgentState 来自哪个 session、对齐到哪个代码基线：

```console
$ cat .agit/agent/state/_session.json
```

## 接着看

[Demo 03](../03-facts/) — fact 的证据会过期。
