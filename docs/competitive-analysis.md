# AgentGit 竞品分析与差异化策略

> 日期：2026-07-09
> 分析对象：Shepherd (Stanford/Northeastern)、Zed Restore Checkpoint、Claude Code `/rewind`
>
> **信息来源说明**：本机 `~/.claude` 目录结构为直接验证。Shepherd 与 Zed 的细节来自 web 搜索摘要——`WebFetch` 对 arxiv.org / zed.dev / github.com 均被域名策略拦截，**未能精读原文**。文中标注 ⚠️ 的结论是本报告的承重墙，建议有人手动过一遍 arXiv 2605.10913 原文确认。

---

## 0. 一句话结论

> **三个竞品做的都是「往回走」（undo / replay），我们要做的是「往旁边走」（transfer / merge）。这是两个正交的轴。**
>
> 更关键的是：Shepherd 为了做好「往回走」所做的核心技术选择（KV-cache 精确复用），在架构上**排斥**「往旁边走」所需要的表示。这不是它没顾上，是它不能。

---

## 1. 竞品逐个拆解

### 1.1 Shepherd —— 唯一的真竞品

| 项 | 内容 |
|---|---|
| 形态 | 论文 + 开源 Python 库（early alpha，API 会变） |
| 出处 | arXiv 2605.10913，Northeastern + Stanford |
| 作者 | Simon Yu, Derek Chong, Ananjan Nandi, Dilara Soylu, Jiuding Sun, **Christopher D. Manning**, Weiyan Shi |
| 仓库 | `github.com/shepherd-agents/shepherd`（已上 GitHub trending） |
| 官网 | shepherd-agents.ai |
| 传播语 | "Stanford just built Git for AI agents" |

**它做什么。** 把 agent 的一次执行变成一条可逆的、Git 式的 execution trace。每个 model action / tool call / environment change 都是一个结构化 event，也就是一个 commit；fork 开一条 branch；checkout 回到任意历史状态，且**状态精确复原**。

**它捕获什么。** 这是它最狠的地方——不只是文件，而是 **agent 进程 + 文件系统 + KV cache** 一起。开发服务器、数据库、装好的包，全在里面。实现上是 copy-on-write fork，比 `docker commit` 快约 5 倍，replay 时 KV-cache 复用率 >95%。

**它的原语。** ⚠️ 四个，作用在 scope 上：

- `emit` —— 向 scope 的事件流写入一个 effect
- `fork` —— 开一个 copy-on-write 的子 scope
- `merge` —— **把子 scope 的 effects 传播回父 scope**
- `discard` —— 丢弃子 scope，不影响父 scope

**⚠️ 注意 `merge` 的语义。** 它是**父子之间的 effect 传播**，本质是嵌套事务 / structured concurrency 的 commit，**不是三方合并，没有冲突解决**。子 scope 持有一个 COW overlay，按构造就不会和父冲突。它无法表达「Alice 的 agent 认为字段叫 `user_id`，Bob 的 agent 认为叫 `uid`」这种**平级冲突**。

**它面向谁。** Meta-agent —— 监督、优化、训练其他 agent 的 agent。论文的三个用例全是这个视角：

1. supervisor meta-agent 防止并行 coding agent 打架，CooperBench pass rate 28.8% → 54.7%
2. counterfactual optimization meta-agent 修复 agent workflow，Terminal-Bench 2.0 上超过 MetaHarness 12.8%，wall-clock 低 58%
3. training meta-agent 挑 fork point 改善 long-horizon RL 的 credit assignment，GRPO uplift 翻倍

这是一篇 **ML infra 论文**，卖点是 benchmark 数字，用户是研究者和 meta-agent 作者。

**它的边界。**

- **单机。** macOS Seatbelt / Linux Landlock 沙箱，container-gated 模式
- **无远程。** 没有 push / pull / clone。trace 不跨机器、不跨人
- **无平级 merge、无 diff、无冲突呈现**
- **侵入式。** 是一个你要把 agent 写进去的 Python substrate（3.11+），不是套在现有 runtime 外面的工具
- Early alpha，API 不稳定

---

### 1.2 Zed Restore Checkpoint —— 一个 undo 按钮

**它做什么。** Agent panel 里，模型每次编辑，那条消息顶部就出现一个 "Restore Checkpoint" 按钮，把**代码库**恢复到这条消息之前的状态。中途打断 agent 时按钮也会出现——这个设计很务实，因为你打断往往正是因为发现它跑偏了。

**实现。** Git backed（底层用 git 做 worktree 快照）。

**它恢复什么。** **只有文件。** 对话 thread 不回滚——thread 独立持久化，重启编辑器也还在，Thread History 里能翻出来。所以 Zed 的模型是「代码可回滚，上下文只进不退」。

**已知问题。**

- [Issue #28676](https://github.com/zed-industries/zed/issues/28676)：**checkpoint restore 是不可逆的破坏性操作**，restore 之后没有 undo/redo。恢复前若没 commit 到 git，工作直接没了
- [Issue #47117](https://github.com/zed-industries/zed/issues/47117) / [#36092](https://github.com/zed-industries/zed/issues/36092)：按钮经常找不到 / 不显示
- [Issue #47907](https://github.com/zed-industries/zed/issues/47907)：Dev Container 下按钮消失

**边界。** 线性、单 thread、纯本地、不可命名、不可导出、不可分享。没有 diff，没有 merge。

---

### 1.3 Claude Code `/rewind` —— 一个更好的 undo 按钮

**它做什么。** `/rewind`，或者输入框为空时按两下 Esc，弹出菜单列出本 session 你发过的每一条 prompt。每条 prompt 是一个 checkpoint。

**三种恢复模式**（这是三个竞品里唯一把两条时间线拆开的）：

1. 代码 + 对话 一起回滚
2. **只回滚对话，保留当前代码**
3. **只回滚代码，保留当前对话**

模式 2 和 3 值得注意——它说明 Anthropic 内部已经把「context 的时间」和「代码的时间」当成两条可以独立操作的轴了。这离我们的思路只有一步。

**实现（本机直接验证，非文档）。**

```
~/.claude/
├── file-history/
│   └── <session-uuid>/
│       ├── <path-hash>@v1        # 文件全量内容副本
│       ├── <path-hash>@v2
│       └── <path-hash>@v3
├── projects/
│   └── <project-slug>/
│       └── <session-uuid>.jsonl  # 对话 transcript
└── sessions/
    └── <n>.json
```

**结论：不是 git，是按 session 分目录的全量文件副本 + 线性递增版本号（`@v1/@v2/@v3`）。** 网上多篇博客说 checkpoint 存在 `~/.claude/checkpoints/`——**本机不存在这个目录**，该说法至少对当前版本是错的。

**它的边界（官方文档明确写出）。**

- **bash 命令改的文件不追踪。** `rm` / `mv` / `cp` 干掉的东西，rewind 救不回来
- 只追踪当前 session 编辑过的文件；手动改动、并发 session 的改动都不捕获
- 官方定位原话：checkpoints 是本地 "undo"，git 是 "permanent history"，**"checkpoints complement but don't replace proper version control"**
- checkpoint 跨 session 保留，默认 30 天清理（可配）

**边界。** 线性（rewind 即截断，被放弃的分支没有 UI 可达）。无导出、无 merge、无跨 session diff、无跨人分享。`--resume` 只能顺着单条 session 线性接下去——这正是我们场景文档吐槽的那一点。

---

## 2. 横向对比

| | Zed Checkpoint | CC `/rewind` | Shepherd | **AgentGit（目标）** |
|---|---|---|---|---|
| 版本化的单元 | 文件 | 文件 + transcript | 进程 + fs + KV cache | **带出处的结论（claim）** |
| 时间方向 | 往回 | 往回 | 往回 | **往旁边** |
| 分支 | ✗ 线性 | ✗ 线性 | ✓ fork | ✓ fork |
| Diff | ✗ | ✗ | ✗ | ✓ semantic |
| Merge | ✗ | ✗ | △ 仅父子 effect 传播 | **✓ 平级 + 冲突呈现** |
| 冲突处理 | — | — | **按构造不存在** | **核心能力** |
| 跨人 | ✗ | ✗ | ✗ | **✓** |
| 跨机器 | ✗ | ✗ | ✗ | **✓ push/pull/clone** |
| 跨 Project | ✗ | ✗ | ✗ | **✓ 多对多索引** |
| 出处 / 证据 | ✗ | ✗ | ✗ | **✓ 一等字段** |
| 侵入性 | 编辑器内置 | CLI 内置 | 要把 agent 重写进去 | **套在现有 runtime 外** |
| 面向用户 | 单个开发者 | 单个开发者 | ML 研究者 / meta-agent | **团队** |

---

## 3. 最锋利的那把刀：KV-cache 与 merge 在架构上互斥

这是本报告最重要的一段。

Shepherd 的头号技术卖点是 **replay 时 >95% 的 KV-cache 复用**。要做到这一点，context 必须是一段**不可变的、有序的、字节级一致的 token 前缀**——这是 prefix cache 成立的前提。它的 `fork` 之所以廉价，正是因为子 scope 只在父的前缀后面追加，前缀原封不动。它的 `merge` 之所以只能子→父，也是因为一旦你把一个平级 branch 的内容插进来，前缀就变了，**KV cache 当场作废**。

而**语义 merge 要求的恰好相反**：context 必须是一组**结构化的、顺序无关的、可独立归因的 claim**，这样两条 branch 的 claim 才能按 key 对齐、检测冲突、各自带着证据摆到 reviewer 面前。

> **精确 replay 和语义 merge 需要两种互相排斥的 context 表示。Shepherd 选了前者，且选得很深（COW + KV 复用是它全部 benchmark 数字的来源）。它不可能顺手把后者做了——那要推翻它的地基。**

这就是我们的结构性空间。不是「他们还没做」，是「他们做不了」。

---

## 4. 场景覆盖矩阵

把 `使用场景.pdf` 的 8 个场景压到竞品上：

| # | 场景 | Zed | CC | Shepherd | 判定 |
|---|---|:--:|:--:|:--:|---|
| 1 | 新人 clone 团队 context | ✗ | ✗ | ✗ | **无人竞争** |
| 2 | 本地多 Agent 同步 | ✗ | △ | ✓ | 被 Shepherd 覆盖 |
| 3 | 跨人跨时区接力（push/pull） | ✗ | ✗ | ✗ | **无人竞争** |
| 4 | fork 试新方向 | ✗ | △ | **✓✓** | **Shepherd 主场** |
| 5 | merge conflict 带证据 | ✗ | ✗ | ✗ | **无人竞争 · 最难 · 核心** |
| 6 | Agent 跨 Project 共用 | ✗ | ✗ | ✗ | **无人竞争** |
| 7 | log + reset | ✓ | ✓ | ✓ | 人人都有，table stakes |
| 8 | push 前扫密钥 | ✗ | ✗ | ✗ | **无人竞争**（因为他们不 push） |

**读出来的信号：被覆盖的 3 个场景（2、4、7）恰好全是「一个人、一台机器、往回走」。剩下 5 个全是「跨人、跨机器、往旁边走」。**

这不是巧合。这说明我们的产品重心应该**整体右移**——不要在 fork/replay 上跟 Shepherd 拼工程，那是它花了一篇 paper 建的护城河，而且和我们要的东西方向相反。

---

## 5. 差异化策略

### 5.1 定位（三句话）

- **Shepherd** = 让 agent **重来一次**（reversibility · 单机 · 单次 run · meta-agent 视角）
- **Zed / CC rewind** = 让代码**回到从前**（undo · 单人 · 恐慌按钮）
- **AgentGit** = 让一个 agent **学到另一个 agent 学到的东西**（transfer · 跨人 · 跨 project · 带冲突与出处）

前两者是**时间轴上的后退**。我们是**主体之间的横移**。轴不同，所以严格说不正面竞争——**但必须大声说出来，否则每个人的第一句话都是「这不就是 Shepherd 吗」。**

### 5.2 四个护城河，按优先级

**① Provenance（出处）作为一等字段 —— 一切的地基**

Context Schema 里每一条 claim 必须携带证据：`file:line`、命令输出、文档 URL、时间戳、置信度。

三个竞品**没有一个**给结论附证据。这一个字段解锁三件事：

- **merge 可裁决**：冲突时能把「`user_id`，依据 `models/user.ts:12`」和「`uid`，依据一份两年前的文档」并排摆出来，让人/agent 判断
- **陈旧自动失效**：claim 引用的 `models/user.ts:12` 变了 → 自动标记该结论待复核。**这是竞品原理上做不到的**（它们没有指向源头的指针），而且是能单独拿出来讲的杀手级特性
- **信任传递**：clone 别人的 context 时，你信的不是「他的 agent 这么说」，而是背后那条证据

**② 平级 Semantic Merge + 冲突对象**

Shepherd merge 是 child → parent（层级，按构造无冲突）。我们要的是 Alice ↔ Bob（平级，冲突必然发生）。

merge 的产出不是「合并后的 context」，而是一个 **conflict 对象**：两条 claim + 两份证据 + 待裁决标记。这是场景 5，也是整个项目最难、最没人做、最像样的东西。

**③ Remote：push / pull / clone**

三个竞品全部本地。场景 1、3、6 零竞争。

这是产品名里 **"GitHub" 那一半**赚钱的地方——不是版本控制，是**网络效应和资产沉淀**。一个团队攒了两年的 payments context，新人一条命令拿走，这个价值 Shepherd 结构上给不了。

**④ Secret Scan：因为要分享，所以必须有**

场景 8 不是 nice-to-have，是 push 的**准入门槛**。小林 `cat` 了 `.env`，密码进了 context，一 push 一 clone，密码上了全组的机器。

关键认知：**这个护城河的存在，恰恰是因为我们做了 ③。** 竞品没有它，不是因为疏忽，是因为它们不分享。反过来这也重构了我们的叙事——AgentGit 不只是效率工具，是 **context 共享的安全层**。企业采购时这一条会被单独拎出来问。

必须和 push 同期上线，不能排到后面。

**⑤（加分项）Runtime 无关**

Shepherd 要求你把 agent 写进它的 Python substrate。我们的文档写的是「兼容现有 Agent Runtime，无需修改底层系统」。

这是**采纳成本上的代差**。我们可以直接吃 Claude Code 的 `.jsonl`、Zed 的 thread、Cursor 的 session。Shepherd 让你重写 agent，我们让你跑个 CLI。

### 5.3 该放弃什么

**明确不做**：精确 replay、KV-cache 复用、进程快照、容器隔离。

理由有二：一，工程成本极高且是 Shepherd 的主场；二——更重要——**它和语义 merge 在表示上互斥**（见 §3）。同时追两个，schema 会被撕裂。

**该让出场景 4（fork 试新方向）。** 我们的文档吐槽 CC 的 fork「没办法 merge 回去」——但 Shepherd 恰好解决了这个（fork + merge-to-parent）。**不要拿场景 4 当开场白**，那是替对手做宣传。

**开场白用场景 3（跨人跨时区接力）、5（带证据的 merge conflict）、6（跨 Project 共用）。** 干净、无人竞争、且直击痛点。

### 5.4 对 MVP 排期的影响

原本的直觉是先搭 CLI 骨架把 `commit / checkout / log` 跑通。**建议改。**

`commit / checkout / log / reset` 是 table stakes（场景 7，人人都有），做，但它不是故事，也不构成任何壁垒。

**真正该第一个做的是 Context Schema + provenance**，因为 diff、merge、冲突呈现、陈旧失效、密钥扫描——五个差异化特性全部长在它上面。schema 设计得多好，merge 的上限就有多高。

建议的顺序：

1. **Context Schema（带 provenance）** ← 唯一真正属于我们的东西
2. `diff`（语义 diff，schema 的第一个消费者，也是对 schema 的第一次证伪）
3. `merge` + conflict 对象 ← 护城河
4. `commit / checkout / log`（table stakes，穿起来能用）
5. `push / pull / clone` **+ secret scan（同期，不可拆）** ← 第二个楔子
6. 跨 Project 的 Agent 多对多索引

---

## 6. 风险与反制

**Shepherd 加一个 `push` 怎么办。**
短期不会——它的 merge 语义支撑不了平级冲突，受众是 meta-agent 研究者不是团队。但它有 Manning、有 GitHub trending、有「Git for AI agents」这句已经被抢走的话。**反制：不要争「Git for AI agents」这个词，那是它的。我们争的是「context 可归因、可合并、可审计」。** 它的关键词是 fork/replay，我们的关键词是 provenance/merge。

**Anthropic 给 CC 加 `--export-session` / `--import-session` 怎么办。**
那 80% 的场景 2、3 就没了。可能性不低——`/rewind` 已经把「对话时间线」和「代码时间线」拆开了，说明他们在想这件事。**反制：导出/导入是搬运，merge 才是难的。** 没有 schema 和 provenance，导入两份 context 只能拼接或覆盖，冲突还是没人裁决。护城河在 merge，不在传输。

**CC 文档亲口说 "not a Git replacement"。**
这是 Anthropic 刻意让 checkpointing 保持简单。**这是一个缺口，不是我们的护城河**——在有人填上之前才是我们的。要快。

**⚠️ 本报告的承重假设。**
「Shepherd 的 merge 是父子 effect 传播、无冲突解决」这一条撑起了 §3、§4、§5 的大半。它来自搜索摘要，**未经原文精读**。**如果 Shepherd 其实有平级 merge 和冲突呈现，本报告的差异化结论需要大幅重写。** 请优先安排人精读 arXiv 2605.10913 的 method 章节和 `shepherd-agents/shepherd` 的 README/API 文档确认这一点。

---

## 附：来源

- [Shepherd (arXiv 2605.10913)](https://arxiv.org/abs/2605.10913) · [官网](https://shepherd-agents.ai/) · [GitHub](https://github.com/shepherd-agents/shepherd) · [实验代码](https://github.com/shepherd-agents/shepherd-experiments) · [HuggingFace papers](https://huggingface.co/papers/2605.10913)
- [Zed Agent Panel 文档](https://zed.dev/docs/ai/agent-panel) · [Issue #28676 restore 不可逆](https://github.com/zed-industries/zed/issues/28676) · [Issue #47117](https://github.com/zed-industries/zed/issues/47117) · [Issue #36092](https://github.com/zed-industries/zed/issues/36092)
- [Claude Code Checkpointing 官方文档](https://code.claude.com/docs/en/checkpointing)
- 本机 `~/.claude/` 目录结构（直接验证，2026-07-09）
