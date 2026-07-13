# agit

**A Git-compatible CLI for versioning Agent Context + Environment.**

代码通过 Git 对团队公开了，但 Agent 的工作困在私人 session 里——它读过什么、
知道什么、做过什么判断、下一步是什么，别人看不到，也没法复用。agit 把这些变成
可提交、可比较、可合并、可复用的版本化对象。

```
$ agit -a verify
STALE   api/user/id-field-name    [STALE] file:models/user.ts:4 #a937b4a5
                                    ↳ models/user.ts:4 已变更（a937b4a5 → 06208f05）
```

有人改了代码，那条结论不该再被任何 agent 信任——agit 自己发现了。

---

## 核心：被版本化的对象扩展了

Git 假设作者在系统外，只版本化代码。Agent 是系统内持续积累状态的执行者，所以：

```
Git State      = Environment State
AgentGit State = Agent State + Environment State + Relations
```

agit 因此管**两个 git 库 + 一个配对**：

```
agit <git-args>      = 透明作用在你的代码仓库（Environment），行为不变
agit -a <git-args>   = 同构作用在 Agent Store（.agit/agent，独立 git 仓库）
```

- **Agent State**：目标、约束、事实+证据、决定、进度、artifact —— 住在 `.agit/agent/state/`
- **Environment State**：repo identity + commit + stash（覆盖 staged/unstaged/untracked）
- **WorkspaceRevision**：任一库 commit 后自动把「context 的版本」钉到「它所基于的代码版本」

scope 开关只认紧跟 agit 的第一个 token：`agit -a commit` 是 agent scope；
`agit commit -a` 里的 `-a` 是 git 的参数，走代码仓库。

## 一个 fact 一个文件，subject 即路径

```markdown
---
subject: api/user/id-field-name       # 即文件路径，合并的对齐键
tier: reversible
evidence:
- 'file:models/user.ts:4 #a937b4a5'    # 相对 Environment 解析；#... 是采集时的摘要
---
用户标识字段叫 user_id，不是 uid。
```

于是 git 的三方树合并**直接成为语义合并**：

- 两个 agent 改了**同一条结论** → git 报冲突，merge driver 当场重校验双方证据、给确定性建议
- 两个 agent 学到**不同的知识** → git 静默、正确地合并，一方都不丢

而 `#a937b4a5` 这个指向源头的指针，让「证据是否还成立」可被回头验证——这是三个竞品
（Shepherd / Zed checkpoint / Claude Code rewind）原理上做不到的：它们记录状态，不记录出处。

## 装

```bash
./build.sh --release          # 见下方「为什么不是 cargo build」
cp target/release/agit ~/.local/bin/

cd your-repo
agit init                     # 建 Agent Store + 配对基建；clone 后需重跑
```

## 用

```bash
# 从一次 agent session 抽取 AgentState（Claude Code 已实现，Codex 留桩）
agit -a import --summarize            # 证据池 -> fact，走本机 claude 归纳

# 或手写一条带证据的 fact
agit -a new api/user/id -e file:models/user.ts:4 -m '字段叫 user_id。'

agit -a verify        # 证据还对得上吗
agit -a why <subj>    # 出处链 + 当前状态 + 提交历史
agit -a validate      # schema 校验（agit/v1-draft）

agit -a add -A && agit -a commit -m '...'
agit -a push          # 发布 context 给团队
agit -a merge <ref>   # 合并他人 context；冲突带证据裁决
agit -a resolve <subj> --take ours

agit workspace        # Agent↔Environment 配对
agit -a portable      # 导出 PortableState
```

只有几个 agit 原生动词（`init` / `import` / `new` / `verify` / `why` / `resolve` /
`scan` / `validate` / `portable` / `workspace` / `adapter`）。其余一切原样透传 git，
两个 scope 都是。`agit` 不替代 `git`。

## 演示

一个大合集演示，一条叙事串起全部能力（Alice 抽取+发布 → Lin 消费+复用）。命令你自己敲。

```bash
./demo/showcase/setup.sh                 # 搭台：Alice/Lin 的代码仓库 + 一个运行中的 Hub
export PATH="/tmp/agit-demo/bin:$PATH"
# 照着 demo/showcase/README.md 一幕一幕敲

./demo/showcase/rehearse.sh              # 上台前彩排一遍（非交互，16 项检查）
```

覆盖：两库模型、scope 路由与歧义、会话抽取、本机 claude 归纳、手写 fact、证据校验、
**证据过期**、出处链、密钥防线、WorkspaceRevision 配对、schema 校验、PortableState、
Hub 发布/浏览、**证据裁决合并**、一条命令消费、**装回 Claude Code 复用**、Hub claude.md 端点。

详见 [`demo/README.md`](demo/README.md)。

## 安全

- **`agit -a verify` 默认不执行 `cmd:` 证据**——别人分支的 fact 可携带任意 shell 命令，
  `pull` 后跑 verify 就等于执行陌生人代码。必须显式 `--rerun`。
- **抽取时模型只能引用证据池里 agent 真看过的东西**——编造出处在构造上不可能。
- **密钥三道防线**（采集拒绝 / 落盘扫描 / commit·push hook），因为 context 会被 push 和 pull。

## 为什么不是 `cargo build`

依赖树用了 edition2024，`Cargo.lock` 是 v4，需要 cargo ≥ 1.78（Ubuntu 22.04 apt 是 1.75）。
`./build.sh` 自己找到能用的 cargo（优先 `~/.cargo/bin/cargo`），不需要改 PATH。

## 为什么不链接 libgit2

价值在 merge driver。libgit2 / gitoxide / go-git 都是 git 的重新实现，不保证执行
`.git/config` 里的外部 merge driver。所以 agit 一律 shell out 到 canonical git。

## 设计文档

- [`docs/PRD.pdf`](docs/) —— 产品需求
- [`docs/architecture-v2.md`](docs/architecture-v2.md) —— 双库架构决策
- [`docs/agit-v1-schema-draft.md`](docs/agit-v1-schema-draft.md) —— schema 草案
- [`docs/competitive-analysis.md`](docs/competitive-analysis.md) · [`docs/shepherd-survey.md`](docs/shepherd-survey.md) —— 竞品

## 开发

```bash
./build.sh test       # 34 green：unit + cli spine + merge golden + adapter + summarizer
./build.sh --release
```

## AgentGitHub Hub

PRD 的第二个交付物。`agit-hub` 是一个自包含的 Hub：托管 Agent Store（bare git 仓库）、
git smart-http 同步、网页前端 + agent 拉取的 JSON API。

```sh
agit-hub add payments-api && agit-hub serve --port 8177   # 启动
agit -a push http://localhost:8177/payments-api.git       # 发布 context
agit clone http://localhost:8177/payments-api.git         # 一条命令消费
```

详见 [`docs/hub.md`](docs/hub.md)。

## 还没做

- **场景 6（跨 Project 共用 Agent）**：需要多 Project 引用同一 Agent Store。
- **Hub 的权限 / 订阅 / 网页 diff**：第一版是 Registry + Sync，这些未做。
- **Codex adapter**：接口留桩，待样本。
- **精确 schema**：`agit/v1-draft`，待 `codex-session-state-research.md` 收敛。
- **ContextReuse 落到 runtime**：`agit -a export` 出可移植 digest；注入 runtime 续跑待定。

## License

MIT
