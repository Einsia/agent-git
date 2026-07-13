# agit 架构 v2 —— 照 PRD 重写

> 日期：2026-07-11
> 触发：`docs/PRD.pdf`。v1（单库 + `ctx/` 子目录 + 五个新动词）被替换。
> 本文锁定几个一旦落地就很贵的决策，供快速否决。

## PRD 强制的三个变化

1. **被版本化的对象从「一个库」变成「两个库 + 配对」**
   ```
   AgentGit State = Agent State + Environment State + Relations
   ```
2. **CLI 从「五动词 + ctx 锁定透传」变成「`-a` scope 开关 + 双库完整 git 命令面」**
3. **从 session 抽取（Adapter）从「推迟」提到「MVP 核心」**

---

## 决策 A — 两个库的物理形态

| 库 | 是什么 | 物理位置 |
|---|---|---|
| **Environment** | 你现有的代码仓库，原封不动 | 项目根的 `.git`（就是现在的 git 仓库） |
| **Agent Store** | 独立的 git 仓库，装 AgentState | `.agit/agent/`（有自己的 `.git`，自己的 remote） |

- `.agit/` 写进代码仓库的 `.gitignore` —— **agent context 不再污染代码历史**。
  这修掉了 v1 里「ctx/ 提交和代码提交交织在同一条历史线」的隐患。
- `agit -a <args>` ≈ `git -C .agit/agent <args>`。整套 git 命令面（log/show/diff/merge/
  cherry-pick/checkout/fetch/pull/push）**白拿**，一个都不用重写 —— 这就是 PRD 说的「同构操作」。
- Agent Store 有自己独立的 remote，`agit -a push` 把 context 发给团队。

## 决策 B — Agent Store 里的文件布局

```
.agit/agent/                     ← 一个普通 git 仓库
  agent.toml                     ← Agent 身份：id、rules、skill、tool contract
  state/
    goals.md                     ← 目标
    constraints.md               ← 约束
    facts/<subject>.md           ← 事实+证据（= v1 的 claim，subject 即路径）
    decisions/<id>.md            ← 决定
    progress.md                  ← 进度
    artifacts.md                 ← artifact 引用
```

`state/facts/` **就是 v1 的 `ctx/`**：一个 fact 一个文件、subject 即路径、带 evidence + tier。
所以 v1 的 claim / evidence / merge / scan 四个模块原地移植，只是根目录换了。
merge driver 注册在 **Agent Store** 的 `.gitattributes` 上：`state/facts/** merge=agit`。

## 决策 C — Environment 捕获（EnvironmentState）

```
EnvironmentState = repo identity + HEAD commit + stash
```
`stash` **必须覆盖 staged / unstaged / untracked**（PRD 明确要求）。

实现走 plumbing，不动用户工作区：临时 `GIT_INDEX_FILE` 从 HEAD 起，
`add` 全部已跟踪修改 + untracked，`write-tree` 得到一个 stash-tree 的 SHA。
EnvironmentRevision = `{ repo_identity, head_commit, stash_tree }`。

> 目的（PRD 原文）：「避免脱离代码基线传播结论」。
> 这正是 v1 的 staleness —— 现在升级成 AgentState↔EnvironmentState 的显式绑定。

## 决策 D — WorkspaceRevision（JointVersionControl）

「agit commit 固定 EnvironmentRevision，agit -a commit 固定 AgentRevision。
任一 ref 移动后，agit 自动生成 WorkspaceRevision。」

- 存为 append-only 的 `.agit/workspace/log.jsonl`，每条：
  ```json
  {"ts","agent_rev","env":{"repo_identity","head_commit","stash_tree"},"relations","trigger"}
  ```
- **不放进任何 git worktree** —— 否则「写 workspace revision」本身会移动 agent ref，
  触发再写一条，无限递归。放在 git 之外，避免这个环。
- `.agit/workspace/HEAD.json` 指向最新一条。将来同步给 Hub。

## 决策 E — argv 路由（最容易错的地方）

```
agit [SCOPE] <command> [args…]
SCOPE = 第一个 token：缺省 / -e → Environment，-a → Agent
```

- `<command>` 是 agit 原生动词（`init` `import` `export` `verify` `why` `scan` `workspace`）
  → 我们处理。
- 否则 **透明透传**：`spawn`（不是 exec）对应库的 git，继承 stdin/out/err、传播退出码、
  跑完再做 post-hook（ref 动了就写 WorkspaceRevision）。
  用 spawn 不用 exec，是为了跑完还能做 post-hook；继承 stdio 保证 credential helper、
  交互式 prompt、hook 全部照常。

**关键歧义**（PRD 专门点了）：
```
agit -a commit      # SCOPE=agent，第一个 token 是 -a
agit commit -a      # SCOPE 缺省=env，-a 是 git commit 的 -a 参数
```
scope 开关**只认紧跟 agit 的第一个 token**。子命令之后的 `-a` 原样交给 git。

---

## v1 代码的处置

| 模块 | 处置 |
|---|---|
| `claim.rs` | 保留（一个 fact 就是一条 claim） |
| `evidence.rs` | 保留（证据采集/校验/staleness 引擎） |
| `scan.rs` | 保留（密钥扫描） |
| `merge.rs` | 保留，driver 改注册到 Agent Store |
| `gitx.rs` | 泛化：所有操作带一个「哪个库」参数 |
| `cmd.rs` / `main.rs` | 重写：scope 路由 + 透传 + 原生动词 |
| **新增** `scope.rs` | 库发现（代码根、agent store）、Scope 枚举 |
| **新增** `environment.rs` | HEAD + stash(staged/unstaged/untracked) 捕获 |
| **新增** `workspace.rs` | WorkspaceRevision 写入与日志 |
| **新增** `adapter.rs` | session → AgentState（Codex / Claude Code） |

约 960 行领域逻辑存活，重写集中在路由和三个新模块。

## Adapter（已实现，按 Claude Code 真实结构 + Codex seam）

PRD：「Codex、ClaudeCode 等 runtime 只需实现 export、import 和 validate Adapter。」

`src/adapter/` 定义了 `Adapter` trait（`export` / `import` / `validate` / `locate_default`），
按本机核对的 Claude Code jsonl 真实结构做实，Codex 留成显式报「未实现」的桩（不静默）。

抽取分两层：
- **确定性层（已实现）**：session → `SessionIR` → AgentState。目标来自 prompt、artifact 来自
  Write/Edit、**证据候选池**来自 Read/Bash。file: 证据当场对齐当前代码基线算摘要；
  cmd: 证据**只记不跑**（session 里的命令可能有副作用，import 不执行它们）。
- **语义层（seam，暂缺）**：把证据池 + agent 文本归纳成「结论（fact）」与「决定」。
  这一步需要模型，接口已留，MVP 不在闭环里跑它。

CLI：`agit -a import [--from <rt>] [session]`、`agit -a export [--to <rt>] [out]`、`agit adapter`。

已在真实 session（本对话，1517 行）上验证：抽出 33 条目标、118 条命令、捕获环境基线。

## 悬而未决

**AgentState / PortableState 的精确 schema 仍依赖缺失的 `docs/codex-session-state-research.md`。**
当前 `state/` 字段按 PRD 正文枚举落了一版 draft（目标/约束/事实+证据/决定/进度/artifact +
`_evidence_pool.md` + `_session.json`），标 `agit/v1-draft`。拿到研究文档后收敛字段与
`PortableState = AgentSpecRef + AgentStateRef + WorkspaceRevisionRef + HistoryRef` 的精确形态。

**Codex adapter 待样本**：拿到一份 Codex session 样本即可在 `src/adapter/codex.rs` 填三个
方法，上层一行不改。

**Summarizer 待接**：证据池 → fact 的语义归纳（`agit -a new` 手工路径也可先用）。
