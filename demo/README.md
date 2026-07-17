# agit 演示

一个端到端演示:**[`showcase/`](showcase/)**。直接版本化 agent 的原始 session,像普通 git 仓库一样 push/pull;当两个 agent 对同一份代码产生分歧时,将两侧同时复活,令其各自阅读对方代码并自行合并,仅在出现真实冲突时才请求人工裁决。

```sh
./demo/showcase/setup.sh                 # 准备:一个仓库、两条分叉分支,以及两个 agent 各自的会话
export PATH="/tmp/agit-demo/bin:$PATH"
export AGIT_HOME="/tmp/agit-demo/agit-home"   # 演示用的 agent 放这里,不碰你真实的 ~/.agit
cd /tmp/agit-demo/showcase
# 随后按 demo/showcase/讲稿.md 逐幕演示

./demo/showcase/rehearse.sh              # 上台前彩排一遍(需本机具备 claude)
```

四幕:

- **agent 即记忆**:store 位于 `$AGIT_HOME/agents/<aid>/`,由**身份**而非路径定位;代码仓库里只有一个已提交的 `.agit.toml`,告诉队友这个仓库用哪些 agent。
- **原始 session 版本化与密钥防线**:session 转储进 agent 的 store;pre-commit 钩子拦截夹带密钥的提交(store 无论从哪扇门创建,钩子都会装上)。
- **两条分叉分支,两个 agent**:`ratelimit` 在 `feature-a` 新增以 `user_id` 分桶的限流器,`identity` 在 `feature-b` 将 `user_id` 重命名为 `uid`,构成文本合并无法察觉的交叉冲突。
- **merge**:`agit a merge identity` 将两侧 agent 同时复活(只读,各自运行于其所属分支的 worktree),令其通过对话厘清差异,并留下一个可 resume 的合并后会话;仅真实冲突(`user_id` 与 `uid`)会浮现。**模式由身份决定**:两者 aid 不同 ⇒ 只对话,两份记忆都完整保留。

**[`讲稿.md`](showcase/讲稿.md)** 为主持人版本:每步附带屏幕输出、所体现的能力,以及可用的讲解话术。

---

## 核心模型

- **不定义 fact 或 schema**。Claude Code 与 codex 本身会将完整会话转储到磁盘(转录 jsonl、子 agent、工具结果),`agit snap` 将该转储镜像进 agent 的 store。
- **agent 是一段记忆**:一个装满转录的 git 仓库,以**它知道什么**命名(`frontend`、`ratelimit`),而不是以人或仓库命名。身份是 `aid`(`agt_<uuid>`),铸造一次,提交在 store 自己的 `agent.toml` 里。
- **多对多**:一个 agent 在多个仓库里工作,一个仓库容纳多个 agent。store 以 aid 为键存放在 `$AGIT_HOME/agents/<aid>/`,因此改名与发布都只是元数据改动,目录永不移动——这也正是「一个 agent 把上下文带进另一个仓库」得以成立的原因。
- **绑定靠提交**:代码仓库根部的 `.agit.toml` 是提交进版本库的,队友克隆后即可 `agit a track <name>` 取回记忆。
- **协作依托 git**:store 即 git 仓库,push、pull、clone 无需额外实现即可使用。
- **合并由 agent 驱动**:`agit a merge <agent>` 将两侧 agent 复活,令其通过阅读代码厘清差异,仅真实冲突请求人工裁决(由模型驱动,故为非确定性,此为有意设计)。
- **两个运行时对等**:claude-code 与 codex,没有默认值;读取会话的命令用 `--from` 指定,否则用现存的唯一一个,否则询问。
- **密钥防线**:转储完整会话意味着转录中可能含有密钥,故提交与 push 时均会扫描(会话扫描采用高精度规则,不被 UUID 类噪声淹没)。

## 目录

| | 用途 |
|---|---|
| `showcase/` | 端到端演示(`setup.sh`、`讲稿.md`、`rehearse.sh`) |
| `lib.sh`、`state.sh` | 公共库、`agit-state`(查看代码仓库、绑定、各 agent 的当前状态) |
| `hub-e2e.sh` | AgitHub 的端到端测试 |

完整命令参考见 [`docs/使用说明.md`](../docs/使用说明.md)。
