# Shepherd 深度调研报告

> 日期：2026-07-09
> 方法：`curl` 直取 arXiv HTML 全文（196k 字符）+ GitHub README 原文 + 关键词全文普查。所有结论均可溯源到原文段落。
> 与上一份 `competitive-analysis.md` 的关系：那份基于搜索摘要，本份基于原文。**核心论断被证实，若干细节被修正。**

---

## 0. 元信息

| 项 | 内容 |
|---|---|
| 论文 | arXiv **2605.10913**（cs.AI） |
| v1 标题 | *Shepherd: A Runtime Substrate Empowering Meta-Agents with a Formalized Execution Trace* |
| 引用标题 | *Shepherd: Enabling Programmable Meta-Agents via Reversible Agentic Execution Traces*（两个标题并存，v1 与 listing 不一致） |
| 作者 | Simon Yu†, Derek Chong‡, Ananjan Nandi‡（三人 equal contribution）, Dilara Soylu‡, Jiuding Sun‡, **Christopher D. Manning**‡, Weiyan Shi† |
| 机构 | † Northeastern University，‡ Stanford University |
| 年份 | 2026 |
| 代码 | `github.com/shepherd-agents/shepherd`，MIT，PyPI `shepherd-ai` |
| 实验代码 | `github.com/shepherd-agents/shepherd-experiments`（冻结快照） |
| 文档 | `docs.shepherd-agents.ai`（自称 early development preview） |
| 状态 | **early alpha，API 会变**，Python 3.11+，**Windows 不支持**（需 WSL） |
| 投稿 | 含 NeurIPS Paper Checklist，应为 NeurIPS 投稿 |

---

## 1. 一段话摘要

Shepherd 把「agent 的一次执行」变成函数式编程意义上的**一等值**，让另一个 agent（meta-agent）能够 hold / call / copy / rewrite 它。它把执行记录成一条 Git 式的 commit 图，每个 model call、tool call、环境变更都是一个 typed effect（commit）；`fork` 以 copy-on-write 的方式同时复制 **agent 进程 + 文件系统 + 绑定环境**，`discard` 精确回滚，replay 字节级一致因而能命中 LLM 提供商的 prompt cache。

**这是一篇 ML 系统论文，目标读者是 meta-agent 的作者和 RL 研究者，不是团队协作的开发者。**

---

## 2. 它要解决的问题（原文视角）

论文的出发点不是「人和人协作」，而是「**agent 监督 agent**」：

> "As LLM-based agentic systems grow more complex, they increasingly rely on **meta-agents**: higher-order agents that act on other agents, much like managers supervise employees. Yet existing agentic runtimes expose execution only as static environmental states, limiting the kinds of live and post-hoc interventions a meta-agent requires."

它认为现有 runtime 暴露出来的东西（transcript、tool log、snapshot）**都是为了让 worker agent 维持自己的状态**，而 meta-agent 需要的是把 worker 及其执行本身当成一个可读、可倒带、可分支、可改写的结构化对象。

Table 1（原文能力矩阵，我从 SVG 字形还原）：

| Method | Fork FS | Fork Agent | Revert | Replay | Merge |
|---|:--:|:--:|:--:|:--:|:--:|
| Full copy | ● | ○ | ○ | ○ | ○ |
| docker commit | ● | ○ | ○ | ○ | ○ |
| OpenHands [37] | ● | ○ | ○ | ◐ | ○ |
| **AgentGit [22]** | ● | ○ | ○ | ○ | ○ |
| BranchFS [34] | ● | ○ | ◐ | ○ | ● |
| **Shepherd** | ● | ● | ● | ● | ● |

（● 支持 / ○ 不支持 / ◐ 部分；口径是「作为单个 in-process 操作」）

> ⚠️ **注意 Table 1 里的 `AgentGit [22]`。** 这不是我们——这是一篇已经存在的论文。详见 §9。

---

## 3. 编程模型：四个一等公民

论文的骨架是把函数式编程的四个构造映射到 agent 执行上。

### 3.1 Task = 一等函数

用 `@agent` 装饰一个有类型输入输出的 Python 类。函数体可以不写——Shepherd 能从签名综合出一个实现（一次 model call 把 typed input 变成 typed output）。

```python
@agent
def fix(issue: Issue) -> Patch

@agent
def supervise(work: Task[Issue, Patch]) -> Patch
```

`supervise` 是高阶的：它自己是个 task，参数是另一个 task。**所以 meta-agent 只不过是「参数恰好是别的 task 的 task」**，并且可以递归出 meta-meta-agent。

### 3.2 Effect = 代数效应

每个会触及外界的动作被记录为 typed event，append 到一条 effect stream，meta-agent 通过订阅来读。

**三条关键性质：**

1. **意图与结果是两个事件。** 一次 tool call 发两个 effect：发起时（工具名 + 参数）和返回时（结果）。这让 **mid-tool-call 拦截**成为可能。

2. **effect 分三个 reversibility tier**（这个设计很漂亮）：
   - **reversible**（文件系统写、sandbox 状态）→ 通过 scope 原生回滚
   - **compensable**（运行中的服务、数据库写）→ 通过用户提供的 compensation handler 回滚
   - **irreversible**（model call、支付、发邮件）→ **发出即落地**，stream 只能记录以供审计

3. **观测不扰动。** effect stream 是 append-only + immutable，因此 worker 的 stream 是否被 meta-agent 观看，字节完全一致。附录 Table 8 有个专门的 "Context inflation test"：**supervisor 订阅给 worker 增加 0 个 token。**

### 3.3 Scope = region-scoped handler

Scope 是一个绑定环境（sandbox handle、model provider、tool surface），拥有在其中运行的 task 发出的 effect stream。

**四个原语：**

```python
@agent
def supervise(work: Task[Issue, Patch]) -> Patch:
    child = scope.fork()
    result = await work(child)
    if result.failed: child.discard()
    else: scope.merge(child)
    return result
```

- `emit` —— 向 scope 的 stream 写一个 effect
- `fork` —— 开一个 copy-on-write 子 scope
- `merge` —— **把子 scope 的 effects 传播进父 scope**
- `discard` —— 丢弃子 scope，不影响父

`scope.fork()` 把 worker 的**文件系统、进程、绑定**作为单个 COW 步骤一起捕获。实现是 overlay 文件系统虚拟化 + 容器化 sandbox 的原生 checkpoint 能力，藏在统一的 device-layer 接口后面。

> "Discarding a child leaves the parent **byte-identical** to the moment of fork."

Scope 可嵌套：meta-meta-agent 能 fork/observe/resume 一个 meta-agent，而不污染底下的 worker。

### 3.4 Execution Trace = 持久化数据结构

trace 是一张持久化的 Git 式 commit 图。四个 scope 操作直接编译成 git 操作——**这是原文的对照表，不是我的比喻**：

```
scope.emit(effect)    ~  git commit -m "<effect>"
scope.fork()          ~  git checkout -b <child-branch>
scope.merge(child)    ~  git merge <child-branch>
scope.discard(child)  ~  git branch -D <child-branch>
```

两条性质：

- **任意历史状态可达且可 replay。** 按 hash 导航到任意 commit，读到那一刻 worker 的精确状态。定位「回归第一次出现在哪个 commit」「两个兄弟从哪里开始分岔」退化成图遍历。replay 产生字节级精确的重建，meta-agent 只为它真正执行的后缀付费。
- **分岔分支按结构共享存储。** 同一 commit fork 出的两个兄弟，整个前缀按 content hash 共享。并且——

> "any set of branches can be **diffed at the effect level** — letting a meta-agent decide which to merge on the basis of their behavior."

**所以 Shepherd 是有 diff 的**，effect 级别的，跨任意分支。这一点我在上一份报告里说「无 diff」，**错了，此处更正。**

---

## 4. 但 `merge` 到底是什么？（本报告的核心结论）

我对全文做了关键词普查（196k 字符）：

| 关键词 | 出现次数 | 实际语境 |
|---|:--:|---|
| `conflict` | 4 | **全部**在 CooperBench 评测脚本和 meta-agent prompt 里，不是 Shepherd 的能力 |
| `three-way` / `3-way` | 0 | — |
| `semantic merge` | 0 | — |
| `provenance` | 1 | Lean 证明信封的 "kernel-v3 reference provenance"，与结论出处无关 |
| `multi-user` / `multiuser` | 0 | — |
| `secret` / `credential` | 0 | — |
| `cross-project` | 0 | — |
| `push` | 6 | 全是 "push guidance"（inject 工具）、人名 Pushmeet、`git push --force` 举例。**没有一次是 push trace 到远端** |
| `pull` | 0 | — |
| `clone` | 1 | 文件系统 clone 延迟，不是仓库 clone |
| `remote` | 5 | 远程 sandbox 后端（E2B / Modal / Daytona），不是远程仓库 |

**结论：`scope.merge(child)` 是父子之间的 effect 传播，语义等价于嵌套事务的 commit。子 scope 是父的 COW overlay，按构造不会与父冲突。**

三处旁证：

**① 它的产品级 merge 只处理路径不相交的变更。** README 原文：

> "settled once with `select` / `apply` / `release` / `discard` (`apply` **three-way-merges a candidate onto a workspace that already moved on, when their changes are path-disjoint**)."

路径一旦相交，就不 apply。没有冲突呈现，没有裁决。

**② 它的 Lean 证明只覆盖单子分支。** 附录 B 的定理表里：

> `single_child_branch_replay_sound` —— "One-child structural replay soundness for an exact suffix replay model; **this is not a production replay refinement proof**."

而 Non-claims 明确写着，证明信封**不**覆盖：

> "...Docker or sandbox implementations, **meta-git carrier storage**, scheduling, cancellation, retries, recovery, or **multi-branch replay**."

**③ 冲突被外包给了 `git merge-file`。** CooperBench 的评测方式（这是 benchmark 的 harness，不是 Shepherd）：

> "the two patches are merged via `git merge-file` (with a **Qwen 1.5B trivial-conflict resolver**), the merged tree is checked out..."

而且 CooperBench 数据集的构造方式本身就是：

> "every (repo, task, feature-pair) tuple from the public release whose two ground-truth patches **produce a git merge conflict** when applied independently."

Shepherd 的 supervisor 是靠**实时干预避免冲突发生**（inject / handoff / discard），不是靠 merge 解决冲突。它给 meta-agent 的 prompt 里甚至说：

> "Different agents editing the same file is USUALLY fine — they are in separate sandboxes and their patches will be merged by git afterwards. Only call this a conflict if the edits would be irreconcilable at merge time (same lines, different intent)."

---

## 5. 更锋利的一刀：跨 agent 的上下文传输带宽 = 60 个单词

这是我读到最有价值的一段。Shepherd 的 supervisor 只有三个工具，附录 E 给了精确语义：

**`inject`（代码里叫 `steer`）**
worker 的 session 完全不动，orchestrator 往里 append 一条 user message，内容是 meta-agent 的 guidance 字符串。对话历史、tool-call trail、system prompt 全部保留，**所以 prompt cache 继续命中**。

JSON decision schema 限定：`guidance` **≤ 60 words**，`reason` ≤ 20 words。

**`handoff`（代码里叫 `redirect` / `scope-handoff`）**

> "The target worker's current session is aborted and a fresh session is created... **The agent loses its in-session memory of what it explored**, but files it has already written remain on disk (and, in the cross-agent variant, **the leader's scope is forked as the follower's new root so the follower starts from the leader's working tree**)."

**`discard`（代码里叫 `revert`）**
同 handoff（新 session、丢失记忆），外加 OverlayFS 回滚到 pre-run 快照。

### 读出来的意思

Shepherd 跨 agent 传递的是 **working tree**，不是 **context**。跨 agent handoff 时，follower 拿到 leader 的**文件**，然后**从零开始一个新 session，什么都不记得**。

> **Alice 的 agent 花一下午查明白的东西，在 Shepherd 里传给 Bob 的 agent 的方式是：给 Bob 一份 Alice 的工作目录，加一条不超过 60 个单词的提示。**

这恰好是你们 `使用场景.pdf` 场景 3 描述的痛点本身。Shepherd 没有解决它——**Shepherd 的 handoff 就是那个痛点。**

---

## 6. 为什么它做不了语义 merge：架构上的互斥

上一份报告我推测「KV-cache 复用与语义 merge 互斥」。原文直接证实了这个机制：

> "Because the coupled fork **preserves the parent's exact LLM message prefix**, the provider's prompt cache resolves it without modification."

加上 §3.2 的：

> "The effect stream is **append-only and immutable**."

推论链条是干净的：

1. prompt cache 命中要求 token 前缀**逐字节一致**
2. 所以 `fork` 只能让子分支在父的前缀后面**追加**
3. 所以 `merge` 只能是 child → parent（父在 fork 后没动，是 fast-forward 式的）
4. 一旦要把**平级兄弟**的内容插进来，前缀就变了，**KV cache 全部作废**
5. 而 Shepherd 全部性能数字（95% cache reuse、CRO 的 27–58% wallclock 节省、Tree-GRPO 的可行性）都建立在第 4 步不发生

**Shepherd 不是「还没做」语义 merge，是它的性能地基要求它不能做。** 反过来，语义 merge 要求 context 是一组结构化、顺序无关、可独立归因的 claim——这与 append-only 的字节前缀是两种互斥的表示。

这是一个结构性的、而非时间性的空缺。

---

## 7. 性能：这部分是真硬

### Fork / Revert（Table 2，K=4 并发分支，Terminal-Bench 2.0 真实镜像）

| 镜像 | 方法 | Fork | Revert | Storage | Disk(K=4) | RAM |
|---|---|--:|--:|--:|--:|--:|
| 42 MB | Full copy | 5,154 ms | 2,067 ms | 268 MB | 804 MB | 112 MB |
| | Docker commit | 658 ms | 749 ms | 30 KB | 90 KB | 29.8 MB |
| | Modal snapshot | 3,764 ms | 2,260 ms | — | — | — |
| | BranchFS | 266 ms | 360 ms | 12 KB | 48 KB | 22.7 MB |
| | **Shepherd** | **134 ms** | **142 ms** | **10 KB** | **30 KB** | 20.5 MB |
| 5.8 GB | Full copy | 53,462 ms | 25,943 ms | 8.3 GB | 24.9 GB | 910 MB |
| | Docker commit | 725 ms | 828 ms | 30 KB | 90 KB | 30.2 MB |
| | Modal snapshot | 3,160 ms | 2,328 ms | — | — | — |
| | BranchFS | 280 ms | 358 ms | 12 KB | 48 KB | 22.7 MB |
| | **Shepherd** | **143 ms** | **147 ms** | **10 KB** | **30 KB** | 25.7 MB |

**Fork 延迟与镜像大小无关（42 MB → 5.8 GB，134 → 143 ms）。** 在 5.8 GB 镜像上，K 次 fork 是 K×143 ms，全量拷贝是 K×53.5 s——**每分支 192× 加速**。fork 只占一个 agent turn 的 2–3%。

### KV cache

Anthropic **Claude Haiku 4.5**，8 个 Terminal-Bench 2.0 任务：cache-hit rate 从 K=2 起稳定在 **~95%**，距离字节一致的理论上限 5% 以内。

### 观测开销

本地 Docker 每 event 3.1 ms（5%）；E2B Firecracker 113 ms（87%，**被网络 RTT 主导**）。这是个值得注意的现实约束——远程 sandbox 后端下，effect-stream 记录开销会吃掉大部分收益。

### 后端可移植性

Docker（本地/Vultr）、E2B Firecracker、Modal（gVisor，**无 OverlayFS**）、Daytona、Prime Intellect（gVisor，`cp -a` 兜底，**O(n) 于 workdir 大小**）。所有后端支持同一套 scope API，但 Modal/Prime Intellect 的实现明显退化。

---

## 8. 三个实验

### 8.1 Live Supervision（CooperBench）

**背景**：CooperBench [16]（Khatua et al. 2026, arXiv 2601.13295，Diyi Yang 组，标题很直白——*"Why Coding Agents Cannot be Your Teammates Yet"*）记录了一个 **curse of coordination**：并行 coding agent 的成功率**低于**单个 agent，因为彼此看不见也说不着。

**设置**：479 个 pair。两个 Claude Haiku 4.5 worker（opencode harness，OpenRouter 路由）在并行 fork 的 scope 里各做一个 feature；一个 Sonnet 4.6 / Opus 4.7 的 meta-agent 订阅两条 effect stream，每 10 秒轮询一次，用 inject/handoff/discard 干预。三个 E2B sandbox（每 worker 一个独立 OverlayFS，外加一个共享 relay sandbox 跑 HTTP message bus）。

**结果**：

| 条件 | Pair pass rate | Wallclock / pair |
|---|--:|--:|
| solo（单 agent 顺序做两个 feature，天花板） | 57.2% | 28.4 min |
| coop（两个并行 + peer-to-peer 消息，无监督） | 28.8% | 19.8 min |
| + Sonnet 4.6 meta-agent | 45.3% | 21.2 min（+1.4 meta 开销） |
| **+ Opus 4.7 meta-agent** | **54.7%** | 24.2 min（+4.3 meta 开销） |

Opus 补上了 28.4 个百分点缺口的 **91%**。

### 8.2 Counterfactual Replay Optimization（CRO）

**方法**：维护一个 workflow variant 池及其 trace。proposer 分析 trace 找 failure mode，提出候选 edit，每个 edit 配一个必须修好的 `fix set` 和一个不许退化的 `guard set`。Shepherd 在**受影响的第一个 commit 处 fork**，只 replay 后缀。

**对比**：GEPA [2]、MetaHarness [19]（Lee et al., 含 Chelsea Finn / Omar Khattab）。executor 用 GPT-5.4-mini，meta-optimizer 用 GPT-5.4。

| | HoVer | MATH | IFBench | LiveCodeBench | TB-2 |
|---|--:|--:|--:|--:|--:|
| Baseline | 43.7 | 60.7 | 42.4 | 30.7 | 31.2 |
| GEPA | 43.7 (67min) | 74.0 (20) | 50.1 (50) | 48.7 (73) | 31.2 (157) |
| MetaHarness | 77.8 (235) | 79.3 (101) | **52.3** (126) | 40.0 (217) | 31.2 (173) |
| **CRO** | **79.4** (120) | **80.0** (42) | 51.3 (82) | **51.0** (117) | **35.2** (73) |

5 个数据集赢 4 个，wallclock 比 MetaHarness 省 27–58%。IFBench 输 1.0 分（在一个标准差内），但 MetaHarness 多花 37% 时间。

LiveCodeBench 上，computation reuse 从冷启动的 ~1% 涨到后期 **>60%**。

### 8.3 Meta-Agent Guided Tree-RL

meta-agent 在 rollout 中挑 fork 点，从该状态 fork 出 K 个兄弟分支，得到细粒度的 step-level credit。G=8 roots，K=4 siblings。用 tinker 训练，数据是 Endless Terminals（2,492 → 过滤掉 pass@8=1.0 的，剩 442/530 个任务），留出 Terminal-Bench 2.0（89 任务，avg@5，5 seeds）。

| | Qwen3.5-35B-A3B | Nemotron-3-Super-120B-A12B |
|---|--:|--:|
| Base | 26.1% ±4.21 | 30.3% ±3.62 |
| Flat GRPO | 34.2% ±4.05 | 33.8% ±3.41 |
| **Tree-GRPO** | **39.4%** ±3.87 | **37.2%** ±3.19 |

比 Flat GRPO 提升 +5.2 / +3.4 个点。

> **注意**：早前搜索摘要里说的「Terminal-Bench 2.0 上超过 MetaHarness 12.8%」「doubling GRPO's uplift」，与论文原文数字**对不上**。以原文为准。

---

## 9. ⚠️ 名字已经被占了：AgentGit [22]

Shepherd 的 Table 1 和 Related Work 里，引用了一个叫 **AgentGit** 的已有系统：

> **[22]** Li et al. [2025] Yang Li, Siqi Ping, Xiyu Chen, Xiaojian Qi, Zigan Wang, Ye Luo, and Xiaowei Zhang. **AgentGit: A Version Control Framework for Reliable and Scalable LLM-Powered Multi-Agent Systems**, November 2025. arXiv:2511.00628.

摘要（原文）：

> "We present **AgentGit**, a framework that brings Git-like rollback and branching to MAS workflows. Built as an **infrastructure layer on top of LangGraph**, AgentGit supports **state commit, revert, and branching**, allowing agents to traverse, compare, and explore multiple trajectories efficiently. ... Results show that AgentGit significantly reduces redundant computation, lowers runtime and token usage, and supports parallel exploration across multiple branches..."

**这意味着三件事：**

1. **`AgentGit` 这个名字在学术界已经指向一篇 2025 年 11 月的 arXiv 论文**，而且是同一个赛道（LLM 多 agent 系统的 Git 式版本控制）。用这个名字，等于在别人已有的语义上盖房子——搜索、引用、定位全部会撞车。**建议改名。**

2. 好消息是：它和我们做的**不是一回事**。它是 LangGraph 之上的基础设施层，管的是 **MAS workflow 的 state**（为了减少冗余计算、省 token、做 A/B test 和 prompt 选优），不是人与人之间的 context 协作。它的实验是「通过挑更好的 prompt 来优化目标 agent」。

3. 更好的消息是：**Shepherd 的 Table 1 给它打的分很低**——Fork FS ●，其余 Fork Agent / Revert / Replay / Merge 全 ○。所以即便在「Git for agents」这个赛道里，已有工作也远没做到位。

### 顺带认识一下这个赛道的邻居

| 系统 | 出处 | 做什么 |
|---|---|---|
| **AgentGit** | Li et al., arXiv 2511.00628 | LangGraph 之上的 state commit/revert/branch |
| **BranchFS** | Wang & Zheng, *Fork, Explore, Commit: OS Primitives for Agentic Exploration*, arXiv 2602.08199 | FUSE 文件系统分支。Table 1 里唯一有 Merge ● 的 |
| **OpenHands V1** | Wang et al., MLSys 2026, arXiv 2511.03690 | 多 agent coding 系统的 event-sourced 状态管理 |
| **CooperBench** | Khatua et al., arXiv 2601.13295 | 基准：*为什么 coding agent 还不能当你的队友* |
| **MetaHarness** | Lee et al., arXiv 2603.28052 | 端到端优化 model harness |
| **GEPA** | Agrawal et al., arXiv 2507.19457 | 反思式 prompt 演化 |

**赛道很热，而且全部集中在「让 agent 自己或让 meta-agent 更好地跑任务」。没有一个在做「让人和人通过 agent 的 context 协作」。**

---

## 10. 工程现实：README 揭示的东西和论文不太一样

论文讲的是 meta-agent 和 Lean 语义。README 讲的是一个**「可审阅提案」（reviewable proposal）**模型：

> "a task's implementation can be a sandboxed agent, and its work comes back as a **reviewable proposal** — nothing touches your files until you accept it."

CLI 面：

```bash
shepherd init                  # 把目录变成 Shepherd workspace
shepherd doctor claude         # 检查 claude CLI / 登录 / sandbox
shepherd run list | show | changeset
shepherd run select  <run-ref> # 保留
shepherd run apply   <run-ref> # 合并到已经前进了的 workspace（仅路径不相交）
shepherd run discard <run-ref> # 丢弃
shepherd task show             # 展开权限面
```

**权限模型是签名即权限**，很有意思：

```python
@task
def apply_documented_fix(
    docs:    May[GitRepo, ReadOnly],   # 写入在 OS 层被拒绝
    backend: May[GitRepo, ReadWrite],
    issue:   str,
) -> None: ...
```

在 jailed device 上，grant 被编译成该次运行的可写根，**在 syscall 层强制**（macOS Seatbelt / Linux Landlock）——"refused at the syscall — before the last undo point, not advised and not caught only at a merge gate."

当前切片（P-030 v0.2）：per-binding 整体 ReadOnly/ReadWrite，binding 之间路径不相交，同进程 value-children。**子路径级别的 grant 不在这一版里。**

### 两个值得注意的点

**① Quickstart 直接包了 `claude` CLI 当 agent body。** 需要 `claude` 登录或 `ANTHROPIC_API_KEY`，推荐 `CLAUDE_CODE_OAUTH_TOKEN=$(claude setup-token)`。CooperBench 实验里用的是 **opencode** harness。

这修正了我上一份报告的一个说法：**Shepherd 并非完全「要求你把 agent 重写进去」**——它可以在进程边界外面包住一个现成的 CLI agent。

但关键在于：**它包住的是进程和文件系统，不是 context。** 它看得见 worker 写了什么文件、调了什么工具，看不见 worker *理解*了什么。这就是为什么 handoff 只能传 working tree（§5）。

**② 它自称 early alpha / early development preview。** README 顶部有 IMPORTANT 警告，文档站顶部有 "We don't recommend relying on it for production work yet."

---

## 11. 形式化：Lean 部分到底证了什么

Shepherd 把「生产 runtime」和「被 Lean 机械化的语义对象」分开。

> "The production framework executes ordinary Python tasks, provider SDK calls, shell commands, sandbox operations, retries, scheduling, and carrier storage. **Those executions are not themselves verified.** The verified artifact is a small algebraic-effects trace machine..."

有一套 **claim tier** 体系：profile ∈ {`runtime_only`, `reference_core_a`, `core0`, `core_a`, `core0h`, `extension`}，strength ∈ {`runtime_only`, `reference_validated`, `forward_simulation`, `semantic_adequacy`}。**普通 Python 运行默认 `runtime_only`。**

定理表：

| Profile | 代表定理 | 含义 |
|---|---|---|
| Core-0 | `source_eval_to_machine` / `core0_machine_eval_to_source` | source↔machine 前向模拟 + 受限反向模拟 |
| Core-A | `coreA_machine_eval_to_source` | Core-0 + 直接 abort 不 resume 的 handler 边界 |
| Core-0H | `core0h_source_eval_to_machine` | 确定性两阶段 handler body，**仅前向** |
| — | `trace_monotonic` | 机器执行只追加 trace，不重写前缀 |
| — | **`single_child_branch_replay_sound`** | **单子分支**结构化 replay 可靠性。原文注："this is **not** a production replay refinement proof" |

**Non-claims（原文逐字）：**

> "The proof envelope does not verify arbitrary Python control flow, provider SDK behavior, model outputs, **prompt-cache state**, shell commands, **filesystem mutation correctness**, Docker or sandbox implementations, **meta-git carrier storage**, scheduling, cancellation, retries, recovery, or **multi-branch replay**."

**用一句话说：Lean 证的是一个小的效应轨迹机的语义充分性，且分支部分只到「一个子分支」。多分支 replay、文件系统正确性、prompt cache 状态、trace 存储层，全部不在证明范围内。**

这是很诚实的写法，但也说明——**它的形式化地基本身就是父子结构的，不是平级的。**

---

## 12. 自述局限（附录 A.1）

1. **Proof-of-existence framing.** 三个 case study 都只是「存在性证明」。不宣称 meta-agent 策略最优、不宣称跨模型族/benchmark 稳健、**不宣称这些数字离了 Shepherd 就做不出来**。

2. **监督成本。** live supervision 和 CRO 都假设有一个足够强的 meta-agent（Sonnet 4.6 / Opus 4.7 / GPT-5.4）。**短任务上 meta-agent 的 token 成本可能超过 worker 本身**，而这个权衡的适用区间「we do not characterise here」。

3. **counterfactual replay 假设 edit 与副作用弱耦合。** 如果 edit 碰的是一个影响面很广的组件（比如每一步都用到的工具的 system prompt），后缀就是整条轨迹，**cache 一点也省不下**。每个数据集的冷启动 session 都在这个区间里，2–3 个 session 后才摊销掉。

### 我额外读出来的局限

4. **远程 sandbox 下观测开销吃掉收益。** E2B Firecracker 上 effect-stream 记录 113 ms/event（87% 开销），被网络 RTT 主导。

5. **后端能力不均。** Modal（gVisor）没有 OverlayFS；Prime Intellect 退化到 `cp -a`，O(n) 于 workdir 大小（6 GB rootfs 要 57 s）。「所有后端支持同一套 scope API」在性能上并不成立。

6. **Windows 不支持。**

7. **early alpha，API 不稳定。**

---

## 13. 对我们的意义

### 13.1 它是竞品吗？

**在「Git for AI agents」这个叙事上是，在实际要解决的问题上不是。**

| | Shepherd | 我们 |
|---|---|---|
| 版本化的对象 | agent 进程 + 文件系统 + KV cache | **带出处的结论（claim）** |
| 时间方向 | 往回（revert / replay） | **往旁边（transfer / merge）** |
| 分支关系 | 父子（层级） | **平级（peer）** |
| merge 语义 | effect 传播到父，路径不相交 | **三方语义合并 + 冲突对象** |
| 冲突 | 按构造不存在，外包给 `git merge-file` | **一等公民** |
| 跨 agent 传递 | working tree + **≤60 词提示**，context 丢弃 | **完整 context，带证据** |
| 跨人 | ✗（`multi-user` 出现 0 次） | ✓ |
| 跨机器 | ✗（无 push/pull/clone） | ✓ |
| 跨 Project | ✗（`cross-project` 出现 0 次） | ✓ |
| 密钥 | ✗（`secret`/`credential` 出现 0 次） | ✓ |
| 面向谁 | meta-agent 作者、RL 研究者 | **团队里的人** |

### 13.2 三条不能动摇的结论

**① 不要争「Git for AI agents」这个词。** Manning 挂名、GitHub trending、Lean 证明、192× fork 加速——这个位置守得很死。而且它已经用 `scope.emit ~ git commit` 这张对照表把 git 隐喻用尽了。

**② 我们的空缺是结构性的，不是时间性的。** Shepherd 的 95% KV-cache 复用要求 context 是 append-only 的字节前缀；语义 merge 要求 context 是可归因的 claim 集合。**两者互斥。**它不会顺手把我们的活干了，除非推翻自己的性能地基。

**③ 它的 handoff 就是我们的场景 3。** "The agent loses its in-session memory of what it explored." 这一句是我们整个项目的存在理由，被写在了竞品论文的附录 E 里。

### 13.3 立刻要做的三件事

1. **改名。** `AgentGit` = arXiv 2511.00628（Li et al., 2025-11）。这不是可以商量的，是已经发生的事实。新名字应该指向 **provenance / merge / 团队**，而不是 fork / replay / git。

2. **重写场景 4。** 「fork 出来尝试新方向」——Shepherd 用 COW fork + `discard` 做到了，比我们能做的好一个数量级，而且有 Lean 证明。**这个场景不能当卖点。**同理，场景 2（本地多 agent 同步）被它的 live supervisor 覆盖了一半。

3. **把「结论带证据」写进 Schema 的第一版。** `provenance` 在 Shepherd 全文出现 1 次，还是在 Lean 证明信封的语境里。这是整片赛道的空地。我们的 `使用场景.pdf` 场景 5 里那个例子——`user_id`（依据 `models/user.ts:12`）vs `uid`（依据一份很旧的文档）——**Shepherd 连表达都表达不了**，因为它的 effect 里没有「依据」这个字段。

### 13.4 一个可以借鉴的设计

Shepherd 的 **reversibility tier**（reversible / compensable / irreversible）非常漂亮，而且**可以直接搬到 context 上**：

- **reversible claim** —— 从代码读出来的事实（`models/user.ts:12` 说字段叫 `user_id`）。源文件变了，claim 自动失效重算。
- **compensable claim** —— 从命令输出得出的结论（跑了一遍测试，发现 N+1 查询）。需要重跑才能验证。
- **irreversible claim** —— 人做出的决策（「我们决定用 `user_id`」）。不能靠重放推翻，只能被新的决策覆盖，且必须留审计记录。

这个分层直接给出了「陈旧结论何时自动失效」的判定规则，而且是 Shepherd 自己论证过其价值的结构。

---

## 附：一致性勘误（相对上一份 `competitive-analysis.md`）

| 上一份的说法 | 原文核实后 |
|---|---|
| Shepherd「无 diff」 | **错。** 有 effect 级 diff，跨任意分支："any set of branches can be diffed at the effect level" |
| Shepherd merge 无冲突解决 | **对。** 且有三处旁证（path-disjoint apply / single-child Lean 定理 / 冲突外包给 `git merge-file`） |
| KV-cache 与语义 merge 互斥 | **对，且被原文机制直接证实**："the coupled fork preserves the parent's exact LLM message prefix" |
| Shepherd「要求你把 agent 重写进 Python substrate」 | **部分错。** 它能在进程边界包住现成的 `claude` / `opencode` CLI。但它捕获的是进程+文件系统，不是 context |
| Shepherd 「Terminal-Bench 2.0 上超 MetaHarness 12.8%」 | **数字错**（来自搜索摘要）。原文：4/5 数据集最优，wallclock 省 27–58%；TB-2 上 35.2 vs 31.2 |
| Shepherd 「doubling GRPO's uplift」 | **数字错。** 原文：Tree-GRPO 比 Flat GRPO +5.2（Qwen）/ +3.4（Nemotron）个点 |
| （未提及） | **新增：`AgentGit` 是已有论文 arXiv 2511.00628，名字冲突** |

---

## 附：来源

- Shepherd 论文全文 · [arXiv:2605.10913](https://arxiv.org/abs/2605.10913) · [HTML v1](https://arxiv.org/html/2605.10913v1)
- [github.com/shepherd-agents/shepherd](https://github.com/shepherd-agents/shepherd)（README 原文） · [shepherd-experiments](https://github.com/shepherd-agents/shepherd-experiments)
- [shepherd-agents.ai](https://shepherd-agents.ai/) · [docs.shepherd-agents.ai](https://docs.shepherd-agents.ai/)
- AgentGit · [arXiv:2511.00628](https://arxiv.org/abs/2511.00628)
- BranchFS · [arXiv:2602.08199](https://arxiv.org/abs/2602.08199)
- OpenHands V1 · [arXiv:2511.03690](https://arxiv.org/abs/2511.03690)
- CooperBench · [arXiv:2601.13295](https://arxiv.org/abs/2601.13295)
- MetaHarness · [arXiv:2603.28052](https://arxiv.org/abs/2603.28052)
- GEPA · [arXiv:2507.19457](https://arxiv.org/abs/2507.19457)
