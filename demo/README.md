# agit demo

一个大合集演示：**[`showcase/`](showcase/)** —— 一条完整叙事，尽量把所有 feature 串进去，
适合现场演示。命令你自己敲，边敲边讲。

```sh
./demo/showcase/setup.sh                 # 搭台：Alice/Lin 的代码仓库 + 一个运行中的 Hub
export PATH="/tmp/agit-demo/bin:$PATH"
# 照着 demo/showcase/README.md 一幕一幕敲
```

上台前先彩排（非交互跑一遍，确认不翻车）：

```sh
./demo/showcase/rehearse.sh              # 默认路径（不依赖 claude 登录）
SUMMARIZE=1 ./demo/showcase/rehearse.sh  # 连本机 claude 归纳一起验
```

主线：**Alice 干活 → 抽取成带出处的 context → 发布到 Hub；Lin 第二天一条命令复用、核实、
合并、装回自己的 Claude Code。** 四幕覆盖：两库模型、scope 路由与歧义、会话抽取、
本机 claude 归纳、手写 fact、证据校验、**证据过期**、出处链、密钥防线、WorkspaceRevision 配对、
schema 校验、PortableState、Hub 发布/浏览（git smart-http）、**证据裁决合并**、一条命令消费、
**装回 Claude Code 复用**、Hub claude.md 端点。

---

## 目录里还有什么

| | 用途 |
|---|---|
| `showcase/` | 大合集演示（setup.sh 搭台 · README.md 讲稿 · rehearse.sh 彩排） |
| `seed/` | 一个假支付服务的源码，充当被 context 引用的代码基线 |
| `lib.sh` | demo 公共库（建仓库、定位/编译二进制） |
| `state.sh` | `agit-state` —— 在任意 agit 仓库里跑，看「两个库 + 配对」现状 |

> 单 feature 的分课 demo（01–07）已被 showcase 取代、移除；如需回看，在 git 历史里。

## 先回答三个问题

**1. context 存哪？** 两个 git 库：你的代码仓库（Environment，原封不动）+ `.agit/agent`
（Agent Store，独立 git 仓库，装 AgentState）+ `.agit/workspace`（配对）。没有隐藏数据库。

**2. 怎么用？** `agit <git命令>` 作用在代码仓库，`agit -a <git命令>` 作用在 Agent Store。
fact 的 subject 就是文件路径，所以 git 三方合并直接成为语义合并。

**3. 更细的命令与概念？** 见 [`docs/使用说明.md`](../docs/使用说明.md)（详细中文说明书）。
