# agit

**Git for agent context.** 结论带出处，出处会过期，冲突带证据。

```
$ agit verify
STALE   api/user/id-field-name    [STALE] file:models/user.ts:4 #a937b4a5
                                    ↳ models/user.ts:4 已变更（a937b4a5 → 06208f05）
```

有人改了代码。那条结论不该再被任何 agent 信任 —— agit 自己发现了。

---

## 核心想法

**把 claim 的 subject 当成文件路径，一个 claim 一个文件。**

于是 git 的三方树合并直接成为语义合并：

- 两个 agent 改了**同一条结论** → git 报冲突，我们的 merge driver 接管，当场重新校验双方证据
- 两个 agent 学到了**不同的知识** → git 静默、正确地合并，一方都不丢

第二条是重点。换成单个 `ctx.md` 文件，两条不相干的追加会互相冲突；reviewer 随手选一边，就**静默删掉了另一个 agent 的知识**。这是这个产品唯一不能犯的错误。

副产品是：`branch` / `fork` / `reset` / `log` / `push` / `pull` / `clone` 全部白拿，一行代码都不用写。

## 装

```bash
./build.sh --release
cp target/release/agit ~/.local/bin/

cd your-repo
agit init          # 建 ctx/、写 .gitattributes、装 merge driver 与 hook
```

> 为什么是 `./build.sh` 而不是 `cargo build`：依赖树里有 crate 用了 edition2024，
> 而且 `Cargo.lock` 是 v4 格式，需要 **cargo ≥ 1.78**。Ubuntu 22.04 的 apt 版是 1.75。
> `build.sh` 会自己找到能用的那个 cargo（优先 `~/.cargo/bin/cargo`），**不需要你改 PATH 或任何 dotfile**。
> 判据不是版本号，是「它能不能读懂这个 lockfile」。

> `.gitattributes` 会跟着 clone 走，但 `merge.agit.driver` **不会** —— git 有意为之，
> 否则一个仓库就能让 clone 它的人执行任意命令。**所以每次 clone 之后要再跑一次 `agit init`。**

## 用

```bash
# 结论必须带可验证的出处，否则不入库
agit new api/user/id-field-name \
  -e file:models/user.ts:4 \
  -m '用户标识字段叫 user_id，不是 uid。'

agit verify              # 源头还是当初那样吗？
agit why <subject>       # 这条结论从哪来的？
agit diff                # 哪条结论变了？哪条只是证据刷新？

agit add && agit commit -m '...'
agit merge bob           # 冲突里附着双方证据的当场校验结果与建议
agit resolve <subject> --take ours
```

`add` / `commit` / `status` / `diff` / `log` / `branch` / `checkout` / `reset` / `push` / `pull` / `clone`
的语义**和 git 一字不差**，只是范围锁在 `ctx/`。只有五个新动词：`new` / `verify` / `resolve` / `scan` / `why`。

`agit` 不替代 `git`，两个命令并存。`agit add` 只暂存 `ctx/`，绝不替你暂存代码。

## claim 长什么样

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

`#a937b4a5` 是**采集当时那几行的摘要**。`agit verify` 重读源头、重算摘要、比对。
这个字段就是全部魔法所在 —— 竞品记录的是**状态**，我们记录的是**结论 + 指向源头的指针**。

### 证据类型

| locator | 校验方式 | tier |
|---|---|---|
| `file:PATH:LINE[-LINE]` | 重读、重算摘要 | reversible |
| `cmd:COMMAND` | 重跑（**默认不跑**，见下） | compensable |
| `doc:REF@YYYY-MM-DD` | 超过一年判定陈旧 | reversible |
| `human:WHO@YYYY-MM-DD` | 不随代码失效 | irreversible |

三个 tier 借自 Shepherd 的 effect reversibility tier，但作用在**知识**而非副作用上：
代码读出来的事实源变即失效；命令得出的结论要重跑才知道；人做的决策只能被新决策覆盖。

### 裁决是确定性的

merge 冲突时，driver 在**合并那一刻**重新校验双方证据，按「证据状态 → tier」排序给出建议。
同样的三份输入，跑一万次结果一样。模型不进裁决路径 —— 这不是版本控制该有的样子。

双方证据强度相同时，它拒绝猜，把两条都摆给你看。

## 安全

**`agit verify` 默认不执行 `cmd:` 证据。** 一条从别人分支合并进来的 claim 可以携带任意 shell 命令，
`clone` 之后跑一下 `verify` 就等于执行陌生人的代码。必须显式 `--rerun`。

**密钥有三道防线**，因为 context 会被 push 和 clone：

1. **采集时拒绝** —— `.env` / `*.pem` / `id_rsa` / 任何被 `.gitignore` 命中的路径，只留 locator 与摘要，不抄内容
2. **`agit new` 落盘前扫描** —— 正文**和证据快照**一起扫（证据会把源文件内容抄进来，这一点容易漏）
3. **pre-commit 与 pre-push hook** —— 有人绕过前两道，push 那一刻还有一道

Shepherd / Zed checkpoint / Claude Code `/rewind` 都没有这个。不是疏忽 —— 是它们不分享 context。

## 演示

八个**互相独立**的动手 demo。**没有演示脚本，命令全部由你自己敲。**
`setup.sh` 把仓库搭到起始状态，README 告诉你逐条敲什么、预期看到什么、为什么。

**先读 [`demo/README.md`](demo/README.md)** —— 它回答「context 存在哪、怎么管理、agit 动了哪四个地方」。

```bash
./demo/04-merge/setup.sh           # 建仓库（二进制不在会自动编译）
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/04-merge
# 然后照着 demo/04-merge/README.md 一条一条敲

agit-state                         # 任何时候：现在是什么样？
./demo/verify.sh                   # 机器核对：README 没撒谎
```

| # | 回答什么问题 | 文档场景 |
|---|---|:--:|
| [01-init](demo/01-init/) | agit 在磁盘上装了什么？ | — |
| [02-claim](demo/02-claim/) | 一条 claim 是什么？为什么「没有出处的结论不入库」？ | — |
| [03-stale](demo/03-stale/) | 三个月前的结论今天还能信吗？ | — |
| [04-merge](demo/04-merge/) | 两个 agent 结论矛盾怎么办？**不矛盾呢？** | 5 |
| [05-diff](demo/05-diff/) | 哪条结论真的变了，哪条只是补了证据？ | 5 |
| [06-fork-reset](demo/06-fork-reset/) | 试新方向 / 从坏合并里回来（agit 写了 0 行代码） | 4、7 |
| [07-secrets](demo/07-secrets/) | agent 读了 `.env`，push 出去怎么办？ | 8 |
| [08-remote](demo/08-remote/) | 跨人、跨时区、新人上手 | 1、2、3 |

只有 4 分钟的话：**04 → 03 → 07**。

## 还没做

**场景 6（Agent 跨 Project 共用）没做。** v0 把 `ctx/` 放在代码仓库里，agent 因此绑死在 project 上。
要让前端和后端共享同一个 `api-agent`，需要独立的 agent 仓库和 manifest —— 那是 v2 的数据模型。

**从 agent session 里自动抽取 claim 没做。** demo 里的 claim 是手敲的。
真实产品要从 Claude Code 的 `~/.claude/projects/<slug>/<session>.jsonl` 之类的地方抽。
抽取本身要调 LLM，但**证据候选池必须先从 `tool_use` / `tool_result` 里构造好，模型只能从池子里选 locator**——
这样「模型编造出处」从一个事后校验问题，变成构造上不可能。

**subject 对齐没解决，这是唯一可能致命的地方。** 如果 Alice 起的 subject 是 `api/user/id-field-name`，
Bob 起的是 `api/user-id-field`，那是两个文件，git 认为它们毫无关系，**会安静地把两条矛盾的结论都合进来**。
demo 里两人手敲同一个字符串所以看不出问题。真实场景里必须靠 taxonomy + 写入前的 subject 匹配来兜。

## 为什么不链接 libgit2

我们的全部价值在 merge driver。libgit2 / gitoxide / go-git 都是 git 的**重新实现**，
不保证执行 `.git/config` 里的 `merge.<name>.driver` 外部命令。所以 `agit` 一律 shell out 到 canonical git。

## 设计文档

- [`docs/competitive-analysis.md`](docs/competitive-analysis.md) —— Shepherd / Zed checkpoint / Claude Code rewind 的横向对比与差异化
- [`docs/shepherd-survey.md`](docs/shepherd-survey.md) —— Shepherd（arXiv 2605.10913）原文精读

## 开发

```bash
./build.sh test          # 26 个测试：7 个单元 + 19 个端到端
./build.sh --release
```

merge driver 的每条分支都有 golden test：正文相同取并集、一侧未动取另一侧、真冲突给建议、
建议对称（双方对调则翻转）、强度相同拒绝猜、解析失败退回原始冲突。

「裁决是确定性的」是这个东西的全部卖点，没有测试就没有卖点。

## License

MIT
