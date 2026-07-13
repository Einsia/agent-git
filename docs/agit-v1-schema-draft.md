# `agit/v1` Schema —— DRAFT

> 状态：**草案**。PRD 末尾引用的 `codex-session-state-research.md` 尚未到位，
> 本文按 PRD 正文的对象/字段枚举先行落地，供 `agit -a validate` 强制。
> 拿到研究文档后收敛字段与 `PortableState` 的精确形态。
>
> 版本串：`agit/v1-draft`。

## 对象总览（PRD）

```
AgentGit State = Workspace State
               = Agent State + Environment State + Relations
```

| 对象 | 存储 | 本仓库落点 |
|---|---|---|
| Agent | 身份、规则、skill、tool contract | `.agit/agent/agent.toml` |
| AgentState | 目标、约束、事实+证据、决定、进度、artifact 引用 | `.agit/agent/state/` |
| EnvironmentState | repo identity、commit、stash | WorkspaceRevision 里内联 |
| Workspace | Agent / Environment / relations / permissions 的不可变 revision | `.agit/workspace/` |

## Agent（`agent.toml`）

```toml
id = "payments-api-agent"        # 必填，稳定标识
rules = []                        # 行为规则（draft：字符串数组）
skills = []                       # 能力声明
# tool_contract = "..."          # 工具契约（draft，形态待定）
```

## AgentState（`.agit/agent/state/`）

```
state/
  goals.md            # 目标 —— 来自用户 prompt
  constraints.md      # 约束
  progress.md         # 进度
  artifacts.md        # artifact 引用（相对 Environment 的路径）
  decisions/          # 决定（每条一个文件，可选）
  facts/<subject>.md  # 事实 + 证据 —— 一个 fact 一个文件，subject 即路径
  _evidence_pool.md   # 证据候选池（机器抽取的原材料，非结论）
  _session.json       # 溯源：runtime / session_id / cwd / git_branch / 环境基线
```

### fact 文件（承载「事实 + 证据」）

```markdown
---
subject: api/user/id-field-name      # 即文件路径，合并的对齐键
tier: reversible                      # reversible | compensable | irreversible
author: alice
created: 2026-07-13T09:46:14Z         # 秒级 UTC
evidence:                             # 至少一条；否则不入库
- 'file:models/user.ts:4 #a937b4a5'   # 相对 Environment 解析
---

用户标识字段叫 user_id，不是 uid。
```

**硬约束（`agit -a validate` 强制）**：
1. 每个 fact 必须能解析，且 `evidence` 非空。
2. `subject` 合法（`[A-Za-z0-9._-]` 与 `/`，无 `..` / 绝对路径）。
3. 证据 locator 形态合法：`file:PATH:LINE[-LINE]` / `cmd:CMD` / `doc:REF@DATE` / `human:WHO@DATE`。
4. 正文与证据快照里不得含密钥（secret 不得进入 Hub）。

### 证据 locator 与 tier

| locator | 校验 | 隐含 tier |
|---|---|---|
| `file:PATH:LINE[-LINE] [#digest]` | 重读、重算摘要 | reversible |
| `cmd:CMD [#digest]` | 重跑（默认不跑） | compensable |
| `doc:REF@YYYY-MM-DD` | 超 365 天判陈旧 | reversible |
| `human:WHO@YYYY-MM-DD` | 不随代码失效 | irreversible |

## EnvironmentState

```json
{
  "repo_identity": "git@host:org/repo.git",   // remote，或 root:<首提交hash>
  "head_commit": "64c89e2…",
  "stash_tree": "8d15fab…",                    // 覆盖 staged+unstaged+untracked 的 tree
  "dirty": true
}
```

## WorkspaceRevision（`.agit/workspace/`）

`log.jsonl` 每行一条、`HEAD.json` 指向最新：

```json
{
  "ts": "2026-07-13T09:46:14Z",
  "trigger": "agent:commit",         // env:commit / agent:merge / …
  "agent_rev": "<Agent Store HEAD>",
  "env": { EnvironmentState },
  "relations": []                    // draft：Agent↔Environment 隐含；Agent↔Agent 待定
}
```

## PortableState（`agit -a portable` 输出）

PRD：
```
PortableState = AgentSpecRef + AgentStateRef + WorkspaceRevisionRef + HistoryRef(optional)
```

draft 形态：

```json
{
  "agit_version": "v1-draft",
  "agent_spec_ref": "sha256:<agent.toml 的哈希>",
  "agent_state_ref": "<Agent Store HEAD 的 commit sha>",
  "workspace_revision_ref": "<最新 WorkspaceRevision 的指纹>",
  "history_ref": "<_session.json 的 session_id，可选>"
}
```

`AgentStateRef` = Agent Store 的 commit sha（内容寻址，可跨机器复现）。
`HistoryRef` 只存引用：完整聊天记录留在 runtime 侧，**runtime 私有 checkpoint 不能成为跨团队复用的依赖**（PRD）。

## Adapter 契约

runtime 只需实现 `export` / `import` / `validate`（PRD）。见 `src/adapter/`。
Claude Code 已实现；Codex 留桩（待样本）。

## 待收敛（拿到 research 文档后）

- `agent.toml` 的 rules / skills / tool_contract 精确形态
- decisions 的结构化字段（现为自由 markdown）
- `WorkspaceRevisionRef` 的精确指纹算法
- `HistoryRef` 的引用格式（session URI？）
- PortableState 是否需要签名 / 完整性校验
