# agit

**版本化 agent 的原始 session,让团队协作 Agent Context。**

代码通过 Git 对团队公开了,但 agent 的工作困在私人 session 里 —— 它读过什么、
做过什么判断、下一步是什么,别人看不到、没法复用。agit 直接管理 agent 的**原始会话**:
push/pull 那坨,合并时让另一个 agent 读懂对面、自行合并,只有真冲突才问你。

**最小侵入**:不设计 fact、不设计 schema。Claude Code 本来就把整个会话 dump 到磁盘,
我们就版本化那坨。

---

## 核心:两个库 + agent 驱动的合并

```
你的项目/
├── models/user.ts              ← Environment:你的代码,agit 原封不动
├── .gitignore                  ← 自动加入 .agit/
└── .agit/agent/                ← Agent Store:独立 git 仓库
    └── sessions/claude-code/   ← Claude 的原始会话 dump(转录 + 子 agent + 工具结果)
```

- `agit <git 命令>` 作用在你的代码仓库(透明);`agit -a <git 命令>` 作用在 Agent Store。
- Agent Store 就是 git 仓库,所以 push/pull/clone 全白拿。
- **合并靠 agent**:`agit -a reconcile <ref>` 让一个 LLM 读懂对面会话、合成一份统一的工作上下文
  (写进 `CLAUDE.md`,下个会话自动带),**只把真正矛盾的点拎出来问你**。

## 装

```bash
./build.sh --release          # 见下方「为什么不是 cargo build」
cp target/release/agit ~/.local/bin/

cd your-repo
agit init                     # 建 Agent Store;clone 后需重跑
```

## 用

```bash
agit -a sync                          # 把本项目的 Claude session dump 镜像进来
agit -a add -A && agit -a commit -m '...'
agit -a push                          # 发布给团队(push 前扫密钥)

agit clone <url>                      # 同事:一条命令拉团队 Agent Store
agit -a fetch origin
agit -a reconcile origin/main         # agent 读对面会话、合成 CLAUDE.md,真冲突才问你

agit -a scan                          # 扫 session dump 里的密钥
agit workspace                        # Agent↔Environment 配对
agit adapter                          # 列出 runtime adapter
```

原生动词就这些:`init` / `sync` / `reconcile` / `clone` / `scan` / `workspace` / `adapter`。
其余一切原样透传 git(两个 scope 都是)。`agit` 不替代 `git`。

> **scope 歧义**:`agit -a commit`(agent 库)vs `agit commit -a`(代码库,`-a` 是 git 参数)。
> 只认紧跟 agit 的第一个 token。

## 演示

```bash
./demo/showcase/setup.sh
export PATH="/tmp/agit-demo/bin:$PATH"
# 照着 demo/showcase/讲稿.md(主持人版,带屏幕输出)一幕一幕敲

./demo/showcase/rehearse.sh   # 上台前彩排
```

三幕:Alice `sync`+`push` → Bob `clone`+`sync`+`fetch` → `reconcile`(agent 合并,真冲突才问人)。
见 [`demo/README.md`](demo/README.md)。

## 换 LLM 后端(给 Codex 留的口子)

`reconcile` 用到的模型统一走 `src/llm.rs`,后端可插拔:

```sh
export AGIT_LLM=claude               # 默认,本机 claude -p
export AGIT_LLM_CMD="codex exec -"   # 任意从 stdin 读 prompt 的 CLI,现在就能接
```

Codex 的 session dump 解析留了桩(`agit adapter` 可见),拿到格式即可填 `src/adapter/codex.rs`
+ `src/session.rs` 的 `source_dir`。

## 安全

- **dump 全会话 = 转录里可能有密钥**(agent cat 过的 `.env`、打印过的连接串)。所以 commit/push
  前扫密钥。session 文件(jsonl)只用高精度规则,不被 UUID/requestId 那种高熵噪声淹没。
- **reconcile 是非确定性的**(交给模型)—— 这是设计取舍,换来最小侵入 + 真语义合并;能合的都合,
  只有真冲突停下问人。

## 为什么不是 `cargo build`

依赖树用了 edition2024,`Cargo.lock` 是 v4,需要 cargo ≥ 1.78(Ubuntu 22.04 apt 是 1.75)。
`./build.sh` 自己找能用的 cargo(优先 `~/.cargo/bin/cargo`),不用改 PATH。

## AgentGitHub Hub

`agit-hub` 托管 Agent Store(bare git 仓库)、git smart-http 同步、网页浏览。见 [`docs/hub.md`](docs/hub.md)。

## 开发

```bash
./build.sh test               # 13 green:两库/scope/配对/session 密钥/透传/adapter
./build.sh --release
```

## License

MIT
