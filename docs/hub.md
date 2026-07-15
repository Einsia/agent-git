# AgentGitHub Hub

托管团队的 Agent Store(每个就是个 git 仓库,装原始 session),提供同步 + 网页浏览。
一个自包含二进制 `agit-hub`:后端零重量级依赖(std TCP + shell out 到 git),
前端是编译期嵌入的 React SPA(hub-ui/,见 [hub-ui/README](../hub-ui/README.md))。

**Hub = Registry + Sync + 只读渲染,不运行 agent、不做语义合并。**
真正的合并在**消费者本地** `agit -a reconcile`(那里才有 LLM)。

## 起

```sh
./build.sh ui                  # 先编前端(改过前端才需要;dist 已随仓库提交)
./build.sh --release

agit-hub add payments          # 托管一个 agent(建 bare 仓库)
agit-hub token add ci --write  # 发一个写 token —— push 必须带它(见「鉴权」)
agit-hub serve --port 8177     # 启动;默认数据目录 ~/.agit-hub。--private 时读也要 token
```

打开 `http://<机器>:8177/`。前端会用请求的 `Host` 头拼出可直接复制的 clone 地址。

## 鉴权(token)

push(`git-receive-pack`)**必须**带一个写 token,否则 401 —— 这挡掉了"谁都能推、
谁都能覆盖/污染别人会话"的口子。没配任何写 token 时,谁也不能 push(安全默认)。

```sh
agit-hub token add alice --write   # 发写 token(可 push);token 只显示一次
agit-hub token add bob --read      # 只读 token(--private 模式下用于读)
agit-hub token list                # 只列名字与权限,不回显 secret
```

token 存在 `<root>/auth.json`。git 提示输入用户名/密码时,密码填 token(用户名随意);
或走 `Authorization: Bearer <token>`。默认读开放;`serve --private` 时读也要有效 token。

## 发布(Alice)

```sh
cd your-repo
agit -a sync                                   # 先把本项目的 Claude session 镜像进来
agit -a remote add origin http://alice:<token>@<机器>:8177/payments.git
agit -a push -u origin main                    # pre-push 先扫密钥,再走 git smart-http(带 token)
```

## 消费(同事)

```sh
agit clone http://<机器>:8177/payments.git     # 一条命令拉团队 Agent Store
agit -a fetch origin
agit -a reconcile origin/main                  # 本地:agent 读会话、合成 CLAUDE.md,真冲突才问你
```

## 端点

网页路由(`/`、`/agent/<name>`、`/session/<id>`、`.../diff`)全部返回同一个 SPA 外壳,
由前端按 URL 渲染;数据从下面的 JSON API 取。

| 路径 | 内容 |
|---|---|
| `GET /` 及任意网页路由 | React SPA 外壳(`/assets/app.js` + `app.css` 编译期嵌入) |
| `GET /api/agents` | agent 花名册:名字、session 数、最近活动、`host` |
| `GET /api/agent/<name>?page=&q=` | 分页的会话摘要(脊线、provenance、指令、结论、改动文件)+ 提交历史 |
| `GET /api/agent/<name>/session/<id>?at=` | 单条 session 全貌 + revision 列表(`at=` pin 到历史提交) |
| `GET /api/agent/<name>/session/<id>/diff?from=&to=` | 两版的**语义** diff(指令/文件/结论增减,不是 jsonl 行噪声) |
| `/<name>.git/...` | git smart-http(push/pull/clone;push 需写 token) |

**session 脊线(signature):** 每条 session 渲染成一排 tick,按事件类型(prompt/回复/工具/编辑)
定高低与颜色 —— 一眼读出会话的节奏(工具密集?来回讨论?最后一串编辑?),数据来自 ConversationIR。

**provenance:** 每条 session 显示 runtime / 模型 / 分支 / 作者 / 时间(改它的最后一次提交),
consumer 据此判断信任、时效、相关性,再决定要不要 reconcile 进自己的 CLAUDE.md。

## 为什么 Hub 不做合并

合并要读会话、要判断语义 —— 那是 LLM 的活,得在**有代码、有模型**的消费者本地做。
Hub 没有你的代码、按设计不跑 agent,所以只做:托管、同步、只读渲染。
这也避免了"Hub 上跑一个 LLM 处理所有人上传的会话"这种既贵又危险(prompt 注入)的设计。

## 边界

- **只读渲染**:Hub 不合并、不判冲突、不重算任何东西。合并仍在消费者本地 reconcile。
- **不存 secret**:靠发布侧 pre-push hook 拦;但只拦已知格式(见 [风险分析](风险分析.md) §八)。
- **鉴权粒度**:token 是全局的(写 token 能 push 任意仓库);还没有 per-agent ACL / 订阅。
- **搜索上限**:带 `?q=` 时最多扫最近若干条 session(超出会在响应里标记,不静默截断)。
- Windows 下 `git http-backend` 行为不同,未验证。
