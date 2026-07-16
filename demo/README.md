# agit demo

一个大合集演示:**[`showcase/`](showcase/)** —— 直接版本化 agent 的原始 session,
push/pull 那坨,合并时让 agent 读懂对面、自行合并、只有真冲突才问你。

```sh
./demo/showcase/setup.sh                 # 搭台:Alice/Bob 的代码仓库(各带一条会话)+ 团队远端
export PATH="/tmp/agit-demo/bin:$PATH"
# 照着 demo/showcase/讲稿.md 一幕一幕敲

./demo/showcase/rehearse.sh              # 上台前彩排一遍(需本机 claude)
```

三幕:
- **Alice** `agit -a snap`(镜像 Claude 的 session dump)→ `agit -a push`(发布,先扫密钥)
- **Bob** `agit clone` → `agit -a snap` → `agit -a fetch`
- **reconcile** `agit -a reconcile origin/main` —— agent 读两边会话、合成统一上下文写进 `CLAUDE.md`,
  只把真矛盾(`user_id` vs `uid`)拎出来问人

**[`讲稿.md`](showcase/讲稿.md)** 是主持人版:每步带屏幕输出 + 代表的能力 + 可念的话术。

---

## 核心模型

- **不设计 fact/schema**。Claude Code 本来就把整个会话 dump 到 `~/.claude/projects/<项目>/`
  (转录 jsonl + 子 agent + 工具结果),`agit -a snap` 把这坨镜像进 Agent Store。
- **两个库**:你的代码仓库(Environment,原封不动)+ `.agit/agent`(Agent Store,独立 git 仓库,装 session)。
- **协作靠 git**:Agent Store 就是 git 仓库,push/pull/clone 全白拿。
- **合并靠 agent**:`reconcile` 让 LLM 读懂对面会话、合成统一上下文,真冲突才问人(非确定性,设计如此)。
- **密钥防线**:dump 全会话 → 转录里可能有密钥 → commit/push 前扫(session 只用高精度规则,不被 UUID 噪声淹没)。

## 目录

| | 用途 |
|---|---|
| `showcase/` | 大合集演示(setup.sh · 讲稿.md · rehearse.sh) |
| `seed/` | 一个假支付服务的源码(充当代码基线) |
| `lib.sh` · `state.sh` | demo 公共库 · `agit-state`(看两库现状) |

细节命令见 [`docs/使用说明.md`](../docs/使用说明.md)。
