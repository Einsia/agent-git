# 大合集演示 — 一条叙事，串起 agit 的全部能力

> 面向现场演示。命令你自己敲，边敲边讲。全程约 10–15 分钟。
> 上台前先 `SUMMARIZE=1 ./demo/showcase/rehearse.sh` 彩排一遍。

## 舞台

```sh
./demo/showcase/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
```

它建好：**Alice 的代码仓库**（含一份 Claude Code 会话）、**Lin 的代码仓库**、
一个**运行中的 Hub**（`http://localhost:8180/`），并预置了一条 `bob` 的分叉 context 供合并那一幕用。

一句话主线：**Alice 干活 → 抽取成带出处的 context → 发布到 Hub；Lin 第二天一条命令复用、
核实、合并、装回自己的 Claude Code。**

---

# 第一幕 · Alice：从会话到带出处的 context

```sh
cd /tmp/agit-demo/showcase-alice
```

### ① 两个库 —— context 和代码分开，不互相污染
```sh
agit init
agit-state
```
> 讲：agit 建了第二个 git 仓库 `.agit/agent`，`.agit/` 自动进了代码仓库的 gitignore。
> 三处：Environment（你的代码）、Agent Store（context）、WorkspaceRevision（配对）。

### ② scope —— 一套 git 习惯，切两个库
```sh
agit status --short          # 代码仓库
agit -a status --short       # Agent Store
```
> 讲：`agit -a` 前缀切到 context 库。留个悬念：`agit -a commit` 和 `agit commit -a` 不一样，后面会踩到。

### ③ 从一次 agent 会话抽取 AgentState
```sh
agit -a import session.jsonl
cat .agit/agent/state/goals.md
cat .agit/agent/state/_evidence_pool.md
```
> 讲：**确定性**地抽出目标、证据候选池 —— agent 真读过的文件（file: 证据，已对齐当前基线）、
> 真跑过的命令（cmd: 证据，只记不跑）。这是「结论」的原材料。

**可选加演** —— 让本机 claude 把证据池归纳成带出处的 fact：
```sh
agit -a import --summarize session.jsonl
ls .agit/agent/state/facts/
```
> 讲：`--summarize` 调本机 claude。**关键**：模型只能引用证据池里的 locator，编造出处在构造上不可能。
> （加演过就跳过下面④，两者取其一；本脚本主线走手写以求确定性。）

### ④ 也可以手写一条结论（证据指向真实代码）
```sh
agit -a new api/user/id-field-name -e file:models/user.ts:4 -m '用户标识字段叫 user_id，不是 uid。'
```
> 讲：主题即文件路径；`#a937b4a5` 是采集当时那一行的摘要。没有可验证证据的结论会被拒。

### ⑤ 校验 + 出处链
```sh
agit -a verify
agit -a why api/user/id-field-name
```
> 讲：verify 回到代码把每条证据重读、重算摘要、比对。why 给出结论的来龙去脉。

### ⑥ 密钥进不来（因为 context 会被 push/pull）
```sh
agit -a new db/pw -e file:.env:1 -m '连接串在这。'
```
> 讲：第一道防线 —— `.env` 在 denylist 上，直接拒绝，内容根本不进 context。

### ⑦ 提交 context —— 注意 -a 的位置
```sh
agit -a add -A
agit -a commit -m 'alice：用户模型与延迟结论'
agit -a log --oneline
```
> 讲：`agit -a commit` 是 context 库；如果写成 `agit commit -a`，那个 -a 是 git 的参数、提交的是代码。

### ⑧ 配对 + 校验 + 可移植引用
```sh
agit workspace
agit -a validate
agit -a portable
```
> 讲：任一库 commit 后自动把「context 版本」钉到「代码版本」（含覆盖未提交改动的 stash）。
> validate 按 schema 查（有证据、无密钥）。portable 是跨机器复用的引用。

### ⑨ 发布到团队 Hub
```sh
agit -a remote add origin http://localhost:8180/payments-api.git
agit -a push -u origin main
```
> 讲：Agent Store 就是个 git 仓库，push 走真的 git smart-http；pre-push 先扫一遍密钥。
> **现在打开浏览器：** `http://localhost:8180/` —— 团队能直接读 Alice 的目标、带出处的 fact、历史。

---

# 第二幕 · 证据会过期（全场最想让人记住的一点）

```sh
sed -i 's/  user_id: string;/  userId: string;/' models/user.ts
git commit -qam '重命名 user_id -> userId'
agit -a verify
```
> 讲：有人改了代码，没人记得改 context。**agit 自己发现了** —— 那条结论变 STALE，退出码非零（可挂 CI）。
> 三个竞品（Claude Code rewind / Zed checkpoint / Shepherd）原理上做不到：它们记录状态，不记录指向源头的指针。
```sh
agit -a why api/user/id-field-name
```
> 讲：出处链直接指出摘要从 a937b4a5 变成了别的。

---

# 第三幕 · 合并冲突用证据裁决

两个 agent 对「退款状态字段」得出对立结论：一个据代码，一个据 2024 老文档。

```sh
# 队友的分支：据老文档
agit -a checkout -b teammate
agit -a new refund/status-field -e doc:docs/api-v1.md@2024-03-11 -m '退款状态字段叫 status。'
agit -a add -A && agit -a commit -m 'teammate：据 2024 文档'

# Alice 的分支：据代码
agit -a checkout main
agit -a new refund/status-field -e file:services/refund.ts:8 -m '退款状态字段叫 state。'
agit -a add -A && agit -a commit -m 'alice：据代码'

# 合并
agit -a merge teammate
```
> 讲：同一条结论两边都写了 → 冲突。merge driver **在合并那一刻重新校验双方证据**：
> Alice 的是活代码（FRESH），队友的是两年前文档（STALE）。
```sh
cat .agit/agent/state/facts/refund/status-field.md
```
> 讲：冲突文件末尾附了确定性裁决 —— 建议 ours。模型不进裁决路径；证据强度相同时它会拒绝猜。
```sh
agit -a resolve refund/status-field --take ours
```

---

# 第四幕 · Lin：一条命令复用，装回自己的 Claude Code

```sh
cd /tmp/agit-demo/showcase-lin
```

### ⑩ 从 Hub 拉团队 context
```sh
agit clone http://localhost:8180/payments-api.git
```
> 讲：一条命令 —— clone 整个 Agent Store 到 `.agit/agent` 并装好驱动/hook。

### ⑪ 对自己的代码基线核实
```sh
agit -a verify
agit -a why latency/order-service/n-plus-1
```
> 讲：Lin 拿到的不是「Alice 说有 N+1」，而是「Alice 说有 N+1，依据这几行代码，我这边刚复验过」。

### ⑫ 装回 Claude Code —— 下个会话就带着 context
```sh
agit -a export --to claude-code
cat CLAUDE.md
```
> 讲：写进 `CLAUDE.md` 受管区块，Claude Code 每个会话自动加载。幂等，不动你手写的内容。

### ⑬ 或者直接从 Hub 取 Claude Code 就绪的 context
```sh
curl -s http://localhost:8180/agent/payments-api/claude.md
```
> 讲：Hub 也开了这个口子 —— `curl … >> CLAUDE.md` 就把团队 context 塞进任何仓库。

---

## 这一场覆盖的 feature

两库模型 · scope 路由与歧义 · 会话抽取 · 本机 claude 归纳 · 手写 fact · 证据校验 ·
**证据过期** · 出处链 · 密钥防线 · WorkspaceRevision 配对 · schema 校验 · PortableState ·
Hub 发布/浏览(smart-http) · **证据裁决合并** · 一条命令消费 · **装回 Claude Code 复用** · Hub claude.md 端点。

## 收摊

```sh
kill $(cat /tmp/agit-showcase-hub.pid) 2>/dev/null   # 停 Hub
```
