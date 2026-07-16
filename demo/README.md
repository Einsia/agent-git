# agit 演示

一个端到端演示:**[`showcase/`](showcase/)**。直接版本化 agent 的原始 session,像普通 git 仓库一样 push/pull;当两个 agent 产生分叉时,将两侧同时复活,令其各自阅读对方代码并自行合并,仅在出现真实冲突时才请求人工裁决。

```sh
./demo/showcase/setup.sh                 # 准备:一个仓库、两条分叉分支,以及两个 agent 各自的会话
export PATH="/tmp/agit-demo/bin:$PATH"
cd /tmp/agit-demo/showcase
# 随后按 demo/showcase/讲稿.md 逐幕演示

./demo/showcase/rehearse.sh              # 上台前彩排一遍(需本机具备 claude)
```

三幕:

- **原始 session 版本化与密钥防线**:`agit -a snap` 镜像 Claude 的会话转储;pre-commit 钩子拦截夹带密钥的提交。
- **两条分叉分支**:`feature-a` 新增以 `user_id` 分桶的限流器,`feature-b` 将 `user_id` 重命名为 `uid`,构成文本合并无法察觉的交叉冲突。
- **sync**:`agit -a sync bob` 将两侧 agent 同时复活(只读,各自运行于其所属分支的 worktree),令其通过对话厘清差异,并留下一个可 resume 的合并后会话;仅真实冲突(`user_id` 与 `uid`)会浮现。

**[`讲稿.md`](showcase/讲稿.md)** 为主持人版本:每步附带屏幕输出、所体现的能力,以及可用的讲解话术。

---

## 核心模型

- **不定义 fact 或 schema**。Claude Code 本身会将完整会话转储至 `~/.claude/projects/<项目>/`(转录 jsonl、子 agent、工具结果),`agit -a snap` 将该转储镜像进 Agent Store。
- **双仓库**:代码仓库(Environment,保持不变)与 `.agit/agent`(Agent Store,存放会话的独立 git 仓库)。
- **协作依托 git**:Agent Store 即 git 仓库,push、pull、clone 无需额外实现即可使用。
- **合并由 agent 驱动**:`agit -a sync <ref>` 将两侧 agent 复活,令其通过阅读代码厘清差异,仅真实冲突请求人工裁决(由模型驱动,故为非确定性,此为有意设计)。
- **密钥防线**:转储完整会话意味着转录中可能含有密钥,故提交与 push 时均会扫描(会话扫描采用高精度规则,不被 UUID 类噪声淹没)。

## 目录

| | 用途 |
|---|---|
| `showcase/` | 端到端演示(`setup.sh`、`讲稿.md`、`rehearse.sh`) |
| `lib.sh`、`state.sh` | 公共库、`agit-state`(查看两个仓库的当前状态) |

完整命令参考见 [`docs/使用说明.md`](../docs/使用说明.md)。
