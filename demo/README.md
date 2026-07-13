# agit demo（v2 · 双库模型）

七个**互相独立**的动手 demo。**没有演示脚本，命令全部由你自己敲。**

每个 demo 三个文件：`setup.sh`（搭仓库）、`README.md`（你逐条敲什么、预期什么、为什么）、
`expect.txt`（机器核对 README 没撒谎）。

---

## 先回答三个问题

### 1. context 存哪？

**两个 git 库 + 一个配对：**

```
your-repo/                     ← Environment：你的代码，原封不动
├── models/user.ts
├── .gitignore                 ← 含 .agit/
└── .agit/
    ├── agent/                 ← Agent Store：独立 git 仓库，装 AgentState
    │   ├── agent.toml         ← Agent 身份
    │   └── state/
    │       ├── goals.md · constraints.md · progress.md · artifacts.md
    │       ├── facts/<subject>.md      ← 事实 + 证据，一个一个文件
    │       ├── _evidence_pool.md       ← 抽取出的证据候选池
    │       └── _session.json           ← 溯源
    └── workspace/             ← WorkspaceRevision：Agent↔Environment 配对
```

**没有隐藏数据库。全部是 git 对象。**

### 2. 怎么用？

```
agit <git-args>      = 透明作用在你的代码仓库（Environment）
agit -a <git-args>   = 同构作用在 Agent Store
```

scope 开关只认紧跟 agit 的第一个 token：`agit -a commit`=agent；`agit commit -a`=代码（-a 是 git 的参数）。

fact 的 subject 就是文件路径，所以 git 的三方合并直接成为语义合并：
同一结论冲突→driver 用证据裁决；不同知识→静默合并，一方都不丢。

### 3. context 从哪来？

- **抽取**：`agit -a import [--summarize]` 从 Claude Code session 提炼 AgentState
- **手写**：`agit -a new <subject> -e <证据> -m <结论>`

两条路都要求证据落在 agent 真看过的东西上——编造出处在构造上不可能。

---

## 七个 demo

想直接看核心：**04 → 03 → 02**。

| # | 回答什么问题 | PRD |
|---|---|---|
| [01-two-stores](01-two-stores/) | 两个库、scope 开关、`agit -a commit` vs `agit commit -a` | 对象模型 |
| [02-import](02-import/) | 从 session 抽取 AgentState（证据池 + `--summarize`） | ContextManagement |
| [03-facts](03-facts/) | fact 带证据、证据会过期、`verify` / `why` / `validate` | 事实+证据 |
| [04-merge](04-merge/) | 两人 context 合并，冲突用证据确定性裁决 | 团队协作 |
| [05-workspace](05-workspace/) | WorkspaceRevision 配对、`portable` | JointVersionControl |
| [06-secrets](06-secrets/) | 密钥三道防线 | secret 不得进 Hub |
| [07-remote](07-remote/) | push/pull context，同事一条命令复用 | TeamExposure / Reuse |

```sh
./demo/01-two-stores/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/01-two-stores
# 照着 demo/01-two-stores/README.md 一条条敲

agit-state           # 任何时候：现在是什么样（两个库 + 配对）
```

---

## README 不会腐烂

```sh
./demo/verify.sh              # 全部
./demo/verify.sh 04-merge     # 单个
```

`verify.sh` 把每份 README 里 ` ```console ` 块的命令抠出来真跑一遍，断言 `expect.txt`
的每条模式都出现。**README 写的命令必须真能跑，承诺的输出必须真出现。**（当前 64 条命令 / 52 条断言全过。）

---

## 命令速查

| 命令 | 作用 |
|---|---|
| `agit init` | 建 Agent Store + 配对基建（clone 后需重跑） |
| `agit <git…>` / `agit -a <git…>` | 代码仓库 / Agent Store 上的透明 git |
| `agit -a import [--from rt] [--summarize] [sess]` | 从 session 抽取 AgentState |
| `agit -a new <subj> -e <ev> -m <结论>` | 手写一条带证据的 fact |
| `agit -a verify [--rerun]` | 证据还对得上吗（FRESH/STALE/MISSING…） |
| `agit -a why <subj>` | 出处链 + 当前状态 + 提交历史 |
| `agit -a merge <ref>` / `agit -a resolve <subj> --take` | 合并 / 裁决 |
| `agit -a scan` / `agit -a validate` | 密钥扫描 / schema 校验 |
| `agit workspace [log]` / `agit -a portable` | 配对 / PortableState |
| `agit adapter` | 列出 runtime adapter |

---

## 刻意没在 demo 里的

- **场景 6（跨 Project 共用 Agent）**：需要多个 Project 引用同一个 Agent Store，是后续工作。
- **AgentGitHub Hub 网页层**：CLI 阶段用普通 git remote 顶着（Demo 07）。Hub 单独做。
- **Codex adapter**：留桩，待样本。
