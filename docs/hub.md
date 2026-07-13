# AgentGitHub Hub

PRD 的第二个交付物：**GitHub for Agent Contexts**。第一版是 **Registry + Sync**——
不运行 Agent、不保存 secret，只做托管、同步、和「人可读 + agent 同接口拉取」。

一个自包含的二进制 `agit-hub`，零重量级依赖（std 的 TCP + shell out 到 git）。

## 是什么

- **Registry**：托管一堆 Agent Store（bare git 仓库），在 `<root>/<name>.git`。
- **Sync**：git smart-http。`agit -a push/pull http://host:port/<name>.git` 直接可用——
  Hub 是**真正的 git 远端**（内部转交 `git http-backend`）。
- **前端**：服务端渲染 HTML，人可直接阅读 —— 首页、每个 agent 的 Context 页（目标 / 带出处的
  fact / 进度 / 历史）、搜索。
- **API**：同样的数据以 JSON 暴露，agent 通过同一接口拉取。

## 起

```sh
./build.sh --release

agit-hub add payments-api          # 托管一个 agent（建 bare 仓库）
agit-hub add docs-agent
agit-hub serve --port 8177         # 启动，默认 root ~/.agit-hub
```

打开 `http://localhost:8177/`。

## 发布 context（Alice）

```sh
cd your-repo
agit -a remote add origin http://localhost:8177/payments-api.git
agit -a push -u origin main         # pre-push hook 先扫密钥，再走 git smart-http
```

## 消费 context（同事，一条命令）

```sh
cd their-repo
agit clone http://localhost:8177/payments-api.git   # clone 到 .agit/agent + 装驱动
agit -a verify                                       # 对着自己的代码基线复验
agit -a why <subject>                                # 看出处链
```

或者 agent 走 JSON：

```sh
curl http://localhost:8177/api/agent/payments-api    # 结构化 AgentState
```

## 端点

| 路径 | 内容 |
|---|---|
| `GET /` | 首页：托管的 agent + 目标 + fact 数 |
| `GET /agent/<name>` | Context 页；`?q=` 搜 fact |
| `GET /api/agents` | JSON 列表 |
| `GET /api/agent/<name>` | JSON AgentState（目标 + facts + 证据） |
| `/<name>.git/...` | git smart-http（push/pull/clone） |

## 边界（第一版）

- **只读渲染**：Hub 不重跑证据、不重算摘要——证据的新鲜度由**消费者** `agit -a verify`
  对着自己的代码基线判定。这是有意的：Hub 不该假设它有那份代码。
- **不存 secret**：靠发布侧的 pre-push hook 拦（PRD 要求 secret 不进 Hub）。
- **权限 / 订阅**：PRD 列了，本版未做。当前匿名可读可推。
- **URL 里的 host:port 暂时硬编码 localhost:8177**（页面上的发布/拉取提示）。多机部署时需参数化。
- Windows 下 `git http-backend` 行为不同，未验证。

## 还没做（Hub 侧）

- 权限模型、订阅、跨 revision 的网页 diff、full history 浏览
- 多机 / 鉴权部署
- 跨 Project 索引（一个 Project 引用多个外部 Agent）
