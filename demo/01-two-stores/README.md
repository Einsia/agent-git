# Demo 01 — 两个库：代码 与 Agent Context

## 这个 demo 回答什么问题

「agit 把 context 存哪？它和我的代码仓库是什么关系？`agit` 和 `agit -a` 有什么区别？」

## 准备

```sh
./demo/01-two-stores/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/01-two-stores
```

一个还没 agit 的普通代码仓库（假支付服务）。

---

## 步骤 1 — 初始化

```console
$ agit init
```

```text
agit 已就绪。
  Environment : /tmp/agit-demo/01-two-stores
  Agent Store : /tmp/agit-demo/01-two-stores/.agit/agent
```

它建了**第二个 git 仓库** `.agit/agent`，并把 `.agit/` 写进你代码仓库的 `.gitignore`
——**Agent Context 不污染代码历史**。

```console
$ agit-state
```

三个地方：Environment（你的代码）、Agent Store（独立 git 仓库，装 AgentState）、
WorkspaceRevision（两者的配对）。没有隐藏数据库。

---

## 步骤 2 — 默认 scope = 你的代码仓库（透明 git）

`agit <任意 git 命令>` 原样作用在代码仓库上：

```console
$ agit status --short
$ agit log --oneline
```

`.agit/` 被忽略，不会出现在 `agit status` 里。

---

## 步骤 3 — `-a` scope = Agent Store

`agit -a <任意 git 命令>` 同构地作用在 Agent Store 上：

```console
$ agit -a log --oneline
$ agit -a status --short
```

log/show/diff/branch/merge/pull/push 整套 git 命令面，两个库都白拿。

---

## 步骤 4 — 关键歧义（PRD 专门点名）

```console
$ echo "// tweak" >> models/user.ts
$ agit commit -a -m "改代码"
```

这里的 `-a` 是 **git commit 的 `-a` 参数**（提交所有改动），作用在**代码仓库**。

```console
$ echo "id = \"payments-agent\"" > .agit/agent/agent.toml
$ agit -a add -A
$ agit -a commit -m "改 context"
```

`agit -a commit` 里紧跟 agit 的 `-a` 才是 **scope 开关**，作用在 **Agent Store**。

**scope 开关只认紧跟 agit 的第一个 token。** 子命令之后的 `-a` 原样交给 git。

```console
$ agit log --oneline
$ agit -a log --oneline
```

两条历史相互独立：代码一条，context 一条。

---

## 步骤 5 — 它们怎么配对

任一库 commit 后，agit 自动记一条 WorkspaceRevision，把「context 的版本」钉到
「它所基于的代码版本」上：

```console
$ agit workspace
```

详见 [Demo 05](../05-workspace/)。

## 存储模型

| 位置 | 是什么 | 进代码仓库 | 跟着 clone |
|---|---|:--:|:--:|
| 你的代码 | Environment | ✓ | ✓ |
| `.agit/agent/` | Agent Store（独立 git 仓库） | ✗（gitignored） | ✗（自己 push/pull） |
| `.agit/workspace/` | WorkspaceRevision 配对 | ✗ | ✗ |

## 接着看

[Demo 02](../02-import/) — 从一个真实 Claude Code session 抽取 AgentState。
