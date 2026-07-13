# agit demo

八个**互相独立**的动手 demo。**没有演示脚本，命令全部由你自己敲。**

每个 demo 只有三个文件：

| | |
|---|---|
| `setup.sh` | 把仓库搭到起始状态，然后闪开 |
| `README.md` | 你逐条敲什么 · 预期看到什么 · 为什么 |
| `expect.txt` | 机器用来核对 README 没撒谎 |

---

## 先回答三个问题

### 1. context 存在哪里？

**`ctx/` —— 你仓库里的一个普通目录，被 git 跟踪。一个 claim 一个文件。**

```
your-repo/
├── models/user.ts              ← 你的代码
├── services/order.ts
├── ctx/                        ← agit 管的全部东西
│   ├── api/user/id-field-name.md
│   ├── latency/order-service/n-plus-1.md
│   └── refund/flow/services.md
├── .gitattributes              ← ctx/** merge=agit
└── .git/
    ├── config                  ← merge.agit.driver
    └── hooks/{pre-commit,pre-push}
```

**没有隐藏数据库，没有 `~/.agit`，没有额外的对象存储。全部是 git 对象。**

一条 claim 就是一个 Markdown 文件：

```markdown
---
subject: api/user/id-field-name
tier: reversible
author: alice
created: 2026-07-09T10:47:38Z
evidence:
- 'file:models/user.ts:4 #a937b4a5'
---

用户标识字段叫 user_id，不是 uid。
```

`#a937b4a5` 是**采集当时** `models/user.ts` 第 4 行内容的 SHA-256 前 8 位。

### 2. 它怎么管理？

**文件路径就是 claim 的 subject。** 这不是美学选择，是整个设计的支点。

一个 claim 一个文件，于是 git 的三方树合并**直接成为**语义合并：

- 两个 agent 改了**同一条结论** → 同一个文件路径 → git 报冲突 → 我们的 merge driver 接管
- 两个 agent 学到**不同的知识** → 不同的文件路径 → git 静默、正确地合并，一方都不丢

第二条是重点。存成单个 `ctx.md` 的话，两条不相干的追加都落在文件末尾，git 会报一个**假冲突**；
reviewer 随手选一边，就静默删掉了另一个 agent 的知识。

副产品：`branch` / `fork` / `reset` / `log` / `push` / `pull` / `clone`
**全部白拿，一行代码不用写**。

### 3. agit 一共动了哪四个地方？

| 位置 | 内容 | 进仓库 | 跟着 clone |
|---|---|:--:|:--:|
| `ctx/<subject>.md` | claim | ✓ | ✓ |
| `.gitattributes` | `ctx/** merge=agit` | ✓ | ✓ |
| `.git/config` | `merge.agit.driver = <agit 绝对路径> merge-file %O %A %B %P` | ✗ | ✗ |
| `.git/hooks/pre-commit` | `exec <agit> scan --staged` | ✗ | ✗ |
| `.git/hooks/pre-push` | `exec <agit> scan` | ✗ | ✗ |

> **`.git/config` 和 hooks 不跟着 clone 走。** 这是 git 有意的安全设计——否则 clone 一个仓库
> 就等于执行仓库作者写的任意命令。**所以每次 clone 之后必须重跑一次 `agit init`。**
> agit 的每条命令启动时都会检查，没装就警告。

---

## 怎么开始

```sh
./demo/01-init/setup.sh          # 建仓库，打印下面两行给你抄
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/01-init
```

然后打开 `demo/01-init/README.md`，**一条一条自己敲**。

任何时候想知道「现在是什么样」：

```sh
agit-state
```

它打印上面那四个地方的当前内容。跑完的仓库全留在 `/tmp/agit-demo/`，随便翻。

---

## 八个 demo

不需要按顺序。想直接看核心的话：**04 → 03 → 07**。

| # | 回答什么问题 | 文档场景 | 你会亲手做的关键动作 |
|---|---|:--:|---|
| [01-init](01-init/) | agit 往我仓库里塞了什么？ | — | `git config --unset` 掉驱动，看它报警 |
| [02-claim](02-claim/) | 一条 claim 是什么？为什么「没有出处的结论不入库」？ | — | 故意编造出处，看它拒绝落盘 |
| [03-stale](03-stale/) | 三个月前的结论今天还能信吗？ | — | 改一行代码，`agit verify` 变 `STALE`，退出码 1 |
| [04-merge](04-merge/) | 两个 agent 结论矛盾怎么办？**不矛盾呢？** | 5 | 合并两条分支，只得到一条冲突 |
| [05-diff](05-diff/) | 哪条结论真的变了，哪条只是补了证据？ | 5 | 对比 `git diff` 和 `agit diff` |
| [06-fork-reset](06-fork-reset/) | 试新方向 / 从坏合并里回来 | 4、7 | `agit reset --hard`（agit 写了 0 行代码） |
| [07-secrets](07-secrets/) | agent 读了 `.env`，push 出去怎么办？ | 8 | 用 `--no-verify` 绕过 hook，然后被 push 拦下 |
| [08-remote](08-remote/) | 跨人、跨时区、新人上手 | 1、2、3 | 自己 clone 两个仓库，push/pull 一个真实裸远端 |

---

## README 不会腐烂

```sh
./demo/verify.sh              # 全部
./demo/verify.sh 04-merge     # 单个
```

`verify.sh` 把每份 README 里 ` ```console ` 块中以 `$ ` 开头的命令**抠出来真的跑一遍**，
然后断言 `expect.txt` 里列的每条模式都出现在输出里。

**README 写的命令必须真的能跑，承诺的输出必须真的出现。** CI 跑这个。

（写这八份 README 的过程中，它已经抓到我两处假陈述：
一处把 add/add 冲突的 `AA` 写成了 `UU`，一处把 `git rm` 的前置条件搞错了。）

---

## 命令表

`add` / `commit` / `status` / `log` / `branch` / `checkout` / `reset` / `merge` /
`push` / `pull` / `fetch` / `clone` 的语义**和 git 一字不差**，只是范围锁在 `ctx/`。

只有五个新动词：

| 命令 | 做什么 |
|---|---|
| `agit new <subject> -e <证据> -m <结论>` | 落盘前回源头验证证据；验不了、或正文有密钥，就拒绝 |
| `agit verify [--rerun]` | 重新校验所有证据 → `FRESH` / `RECHECK` / `UNVERIFIABLE` / `STALE` / `MISSING` |
| `agit why <subject>` | 这条结论从哪来的：出处链 + 当前状态 + 提交历史 |
| `agit resolve <subject> --take ours\|theirs` | 采纳冲突的某一侧 |
| `agit scan [--staged]` | 扫 claim 正文**和证据快照**里的密钥 |

`agit diff` 是覆盖过的：它做 claim 级的语义 diff，不是按行。

`agit` **不替代 `git`**，两个命令并存。`agit add` 只暂存 `ctx/`，绝不替你暂存代码。

---

## 证据类型

| locator | 校验方式 | tier |
|---|---|---|
| `file:PATH:LINE[-LINE]` | 重读那几行，重算摘要 | `reversible` |
| `cmd:COMMAND` | 重跑（**默认不跑**，见下） | `compensable` |
| `doc:REF@YYYY-MM-DD` | 采集超过 365 天判定陈旧 | `reversible` |
| `human:WHO@YYYY-MM-DD` | 不随代码失效，只被新决策覆盖 | `irreversible` |

> **`agit verify` 默认不执行 `cmd:` 证据。** 一条从别人分支合并进来的 claim 可以携带
> 任意 shell 命令，`clone` 之后跑一句 `verify` 就等于执行陌生人的代码。必须显式 `--rerun`。

三个 tier 借自 Shepherd 的 effect reversibility tier，但作用在**知识**而非副作用上。
它们决定 merge 冲突时的裁决优先级：先比 `证据状态`，相同则比 `tier`，再相同就**拒绝猜**。

---

## 这些 demo 刻意没演什么

**场景 6（Agent 跨 Project 共用）** —— v0 把 `ctx/` 放在代码仓库里，agent 因此绑死在
project 上。要让前端和后端共享同一个 `api-agent`，需要独立的 agent 仓库和 manifest，
是 v2 的数据模型。不在演示里塞半成品。

**从 agent session 自动抽取 claim** —— demo 里的 claim 是手敲的。
真实产品要从 `~/.claude/projects/<slug>/<session>.jsonl` 之类的地方抽，那一步要调 LLM。

**subject 对齐** —— demo 里 Alice 和 Bob 手敲同一个 subject 字符串，所以 git 检测得到冲突。
真实场景里两个 agent 可能起 `api/user/id-field-name` 和 `api/user-id-field`，
git 认为它们毫无关系，**会安静地把两条矛盾的结论都合进来**。

这是整个设计里唯一可能致命的地方。详见根目录 [README](../README.md#还没做)。
