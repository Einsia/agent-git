# Demo 05 — WorkspaceRevision：把 context 钉在代码基线上

## 这个 demo 回答什么问题

「PRD 的 JointVersionControl：一条结论是基于哪个代码版本得出的？怎么保证结论不脱离代码基线传播？」

## 准备

```sh
./demo/05-workspace/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/05-workspace
```

setup 已经提交了一条 context。

---

## 步骤 1 — 看当前配对

```console
$ agit workspace
```

```text
{
  "agent_rev": "...",             ← 当前 AgentState 版本（Agent Store HEAD）
  "env": {
    "head_commit": "...",         ← 它所基于的代码提交
    "stash_tree": "...",          ← 覆盖 staged+unstaged+untracked 的工作树快照
    "dirty": ...
  },
  "trigger": "agent:commit"
}
```

**任一库 commit 后，agit 自动记一条 WorkspaceRevision**，把「context 的版本」钉到
「它所基于的代码版本」上。不用你多敲命令。

---

## 步骤 2 — 提交代码也会生成配对

```console
$ echo "// tweak" >> services/order.ts
$ agit commit -am "code: 调整 order"
$ agit workspace log
```

`log` 里现在有两条：一条 `agent:commit`、一条 `env:commit`。

---

## 步骤 3 — EnvironmentState 覆盖未提交的工作树

PRD 要求 stash 覆盖 staged / unstaged / **untracked**：

```console
$ echo "scratch" > scratch.txt
$ agit -a commit --allow-empty -m "context: 空提交也配对"
$ agit workspace
```

配对里的 `dirty` 变 `true`——即使 `scratch.txt` 还没提交、没跟踪，它也进了 `stash_tree`。
这样「agent 当时基于的确切工作树」被完整记录，结论不会脱离基线。

---

## 步骤 4 — 导出 PortableState

```console
$ agit -a portable
```

```text
{
  "agit_version": "v1-draft",
  "agent_spec_ref": "sha256:...",       ← agent.toml 的哈希
  "agent_state_ref": "...",             ← Agent Store HEAD（内容寻址，可跨机复现）
  "workspace_revision_ref": "sha256:...",
  "history_ref": null
}
```

`PortableState = AgentSpecRef + AgentStateRef + WorkspaceRevisionRef + HistoryRef`（PRD）。
这是跨 runtime / 跨团队复用的可移植引用。完整聊天记录只作引用，runtime 私有 checkpoint
不能成为跨团队复用的依赖。

## 接着看

[Demo 06](../06-secrets/) — 密钥不得进入 context。
