# AgentGitHub Hub

托管团队的 Agent Store(每个就是个 git 仓库,装原始 session),提供同步 + 网页浏览。
一个自包含二进制 `agit-hub`,零重量级依赖(std TCP + shell out 到 git)。

**第一版 = Registry + Sync,不运行 agent、不做语义合并、不保存 secret。**
真正的合并在**消费者本地** `agit -a reconcile`(那里才有 LLM),Hub 只做只读渲染。

## 起

```sh
./build.sh --release

agit-hub add payments          # 托管一个 agent(建 bare 仓库)
agit-hub serve --port 8177     # 启动;默认数据目录 ~/.agit-hub
```

打开 `http://<机器>:8177/`。

## 发布(Alice)

```sh
cd your-repo
agit -a sync                                   # 先把本项目的 Claude session 镜像进来
agit -a remote add origin http://<机器>:8177/payments.git
agit -a push -u origin main                    # pre-push 先扫密钥,再走 git smart-http
```

## 消费(同事)

```sh
agit clone http://<机器>:8177/payments.git     # 一条命令拉团队 Agent Store
agit -a fetch origin
agit -a reconcile origin/main                  # 本地:agent 读会话、合成 CLAUDE.md,真冲突才问你
```

## 端点

| 路径 | 内容 |
|---|---|
| `GET /` | 首页:托管的 agent + session 数 + 最近活动 |
| `GET /agent/<name>` | 会话列表:每条渲染成摘要(指令、结论、改动文件、工具次数);`?q=` 搜转录 |
| `GET /agent/<name>/digest.md` | 会话摘要 markdown(只读;指向本地 reconcile 做真合并) |
| `GET /api/agents` · `/api/agent/<name>` | JSON |
| `/<name>.git/...` | git smart-http(push/pull/clone) |

## 为什么 Hub 不做合并

合并要读会话、要判断语义 —— 那是 LLM 的活,得在**有代码、有模型**的消费者本地做。
Hub 没有你的代码、按设计不跑 agent,所以只做:托管、同步、只读渲染。
这也避免了"Hub 上跑一个 LLM 处理所有人上传的会话"这种既贵又危险(prompt 注入)的设计。

## 边界(第一版)

- **只读渲染**:Hub 不合并、不判冲突、不重算任何东西。
- **不存 secret**:靠发布侧 pre-push hook 拦;但只拦已知格式(见 [风险分析](风险分析.md) §八)。
- **权限 / 订阅 / 网页 diff**:未做,当前匿名可读可推。
- **host:port 在页面提示里未参数化**(多机部署时要改)。
- Windows 下 `git http-backend` 行为不同,未验证。
