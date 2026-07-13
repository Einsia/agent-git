# agit 架构(session 模型)

> 当前架构。之前的"两库 + 手写 fact + 确定性证据合并"(architecture-v2)已废弃。

## 一句话

**版本化对象是 agent 的原始 session。** Claude Code 自己把整个会话 dump 到
`~/.claude/projects/<slug>/`,agit 把这坨版本化、push/pull 同步;合并交给一个 agent
读会话来做,只有真冲突才问人。**不设计 fact、不设计 schema、最小侵入。**

## 两个 git 库 + 一个配对

| | 是什么 | 位置 |
|---|---|---|
| **Environment** | 你的代码仓库,原封不动 | 项目根的 `.git` |
| **Agent Store** | 独立 git 仓库,装 session dump | `.agit/agent`(gitignored) |
| **WorkspaceRevision** | Agent↔Environment 的配对 | `.agit/workspace`(git 之外,避免递归) |

```
agit <git-args>     = 透明 git 作用在 Environment
agit -a <git-args>  = 同构 git 作用在 Agent Store
```
scope 开关只认紧跟 agit 的第一个 token(`agit -a commit` vs `agit commit -a`)。

## Agent Store 里装什么

```
.agit/agent/
  agent.toml                      Agent 身份
  sessions/<runtime>/
    <id>.jsonl                    完整转录
    <id>/subagents,tool-results   子 agent、大工具结果
```
`agit -a sync` 把 runtime 的 session dump 目录**整棵镜像**进来。没有 `state/`、没有 fact。

## 数据流

```
Claude 会话 ──dump──> ~/.claude/projects/<slug>/
                          │  agit -a sync(镜像)
                          ▼
                   .agit/agent/sessions/<rt>/     ──push/pull(git)──> 团队 / Hub
                          │  agit -a reconcile <ref>
                          ▼
              LLM 读两边会话 brief ──> CLAUDE.md(统一上下文) + 冲突清单
```

## 三层,注意哪层确定、哪层不确定

| 层 | 确定性? | 谁做 |
|---|---|---|
| **存储 / 同步**(sync、commit、push/pull) | ✅ 确定 | git |
| **文件层合并**(不同 uuid 的会话并排落入) | ✅ 确定 | git(无文本冲突) |
| **语义合并 / 判冲突**(reconcile) | ❌ **不确定** | LLM(`src/llm.rs`) |

**关键设计:raw session 是唯一真相(git 确定性版本化);CLAUDE.md 只是可重新生成的派生视图。**
把不确定性隔离在最上层、且产物可丢弃重建,是控制风险的主手段(见 [`风险分析.md`](风险分析.md))。

## LLM 后端可插拔

`src/llm.rs`:默认 `claude -p`;`AGIT_LLM_CMD` 接任意 stdin→stdout 的 CLI(Codex 现在就能用);
`AGIT_LLM=codex` 是预留具名口子。所有用模型的地方(目前只有 reconcile)都走这里。

## 密钥

dump 全会话 = 转录里可能有 agent 见过的密钥。commit/push hook 扫 session:
jsonl 只用**高精度规则**(AWS key/连接串/私钥/`password=`…),**关掉泛化熵检测**——否则转录里
海量 UUID/requestId 会疯狂误报。拦不住一般性敏感内容(见风险分析 §八)。

## 模块

| 模块 | 职责 |
|---|---|
| `scope` | 双库发现、scope 路由 |
| `passthrough` | 透明 git 透传(spawn、继承 stdio、传播退出码、post-hook 配对) |
| `session` | `sync`(镜像) + `reconcile`(agent 合并) |
| `adapter` | session 解析(`export`→`SessionIR`),Claude 已实现、Codex 桩 |
| `llm` | 可插拔 LLM CLI 后端 |
| `scan` | 密钥扫描(session 模式) |
| `environment` / `workspace` | EnvironmentState 捕获 / WorkspaceRevision 配对 |
| `commands` | scan / workspace / clone / adapter / write_claude_block |
| `init` | 建 Agent Store + hook |
| `src/bin/agit-hub.rs` | Hub:托管 + git smart-http + 只读渲染 session |

## 明确不做

- 手写 fact / 证据 schema / 确定性证据合并(已删)。
- Hub 上跑 agent / 做合并(合并只在消费者本地,避免贵 + prompt 注入)。
- 精确 replay / KV-cache 复用 / 进程快照(那是 Shepherd 的地盘,见 competitive-analysis)。
