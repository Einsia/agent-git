# AgentGitHub Hub

托管团队的 Agent Store(每个就是个 git 仓库,装原始 session),提供同步 + 网页浏览。
一个自包含二进制 `agit-hub`:后端零重量级依赖(std TCP + shell out 到 git),
前端是编译期嵌入的 React SPA(hub-ui/,见 [hub-ui/README](../hub-ui/README.md))。

**Hub = Registry + Sync + 只读渲染,不运行 agent、不做语义合并。**
真正的合并在**消费者本地** `agit -a reconcile`(那里才有 LLM)。

## 起

```sh
./build.sh ui                       # 先编前端(改过前端才需要;dist 已随仓库提交)
./build.sh --release

agit-hub user add alice --admin     # 第一步:建人。密码交互式问,**不上 argv**
agit-hub add payments --owner alice # 托管一个 agent(建 bare 仓库);默认 private
agit-hub token add ci --user alice --agent payments --write --ttl-days 90
agit-hub serve --port 8177          # 启动;默认数据目录 ~/.agit-hub,**只听 127.0.0.1**
```

打开 `http://127.0.0.1:8177/`。前端会用请求的 `Host` 头拼出可直接复制的 clone 地址。

## 权限模型

一句话:**每个 agent 各自**有 owner、可见性与成员表;**默认 private**;
所有入口(JSON API、git smart-http、CLI)都过同一个判定 [`agit::hub::acl::decide`](../src/hub/acl.rs)
—— 一个 `(caller, agent, action) -> Allow/Deny(原因)` 的纯函数,穷举测试过。

| | 读 | 写(push) | 管理(改可见性/成员/改名/删) |
|---|---|---|---|
| 匿名 | 只有 public | ✗ | ✗ |
| 登录用户(无授权) | 只有 public | ✗ | ✗ |
| 成员 read / write / admin | ✓ | write 起 | admin 起 |
| owner | ✓ | ✓ | ✓ |
| 站点管理员(`user add --admin`) | ✓ | ✓ | ✓ |

**两种凭据**:

- **cookie 会话**(给人):`POST /api/login` 拿 `HttpOnly; SameSite=Lax` 的会话 cookie
  (有 TLS 时加 `Secure`),256 bit 随机、服务端存摘要、12 小时过期、登出立即失效。
  密码用 **argon2id + 每人一把盐**存在 `<root>/users.json`(0600),参数跟着 hash 一起存,
  以后调参不会把老用户锁在门外。
- **token**(给 git 与脚本):`Authorization: Bearer <token>`,或 git 提示密码时填它(用户名随意)。
  token 可**绑定单个 agent**、可设 TTL、可吊销;服务器只存它的 sha256 摘要,明文只显示一次。

> **token 是权限的上界,不是权限的来源。** 实际权限 = token 的 scope ∩ 属主自己的权限。
> 只读 token 落在管理员手里也只能读;写 token 落在只读成员手里也还是只读。
> 管理动作(删库/改可见性/发 token)**一律不接受 token** —— 必须是本人登录会话。
> 属主被删,他的 token 立刻作废。

```sh
agit-hub token add ci --user alice --agent payments --write --ttl-days 90
agit-hub token list                # 列 id/属主/绑定/scope/过期/最近使用,不回显 secret
agit-hub token rm tok_abc123def456 # 吊销
```

## 对外暴露

默认**只听 127.0.0.1**:Hub 装着团队的全部转录,"装上就暴露在办公网"不能是默认值。

```sh
agit-hub serve --host 0.0.0.0 --tls --trusted-proxy 10.0.0.1   # 前面挂 nginx/caddy 终结 HTTPS
agit-hub serve --host 0.0.0.0 --insecure                        # 明文对外(它会告诉你代价)
```

非环回地址 + 没有 TLS + 没有 `--insecure` → **拒绝启动**,并说清为什么(密码和 token 会明文过网线)。
挂反代时务必给 `--trusted-proxy <代理IP>`:否则代理后所有人共用一个 per-IP 限流配额、互相挤下线;
而没声明代理时 Hub **不信** `X-Forwarded-For`(谁都能伪造它)。

## 审计

`<root>/audit.log`,JSONL、只追加(轮转交给 logrotate)。记 login/建库/push/fetch/成员变更/
可见性变更/删库/发 token/吊销,**以及被拒绝的请求**——"谁试过但没进去"往往比"谁进去了"更有用。
`GET /api/audit?agent=&limit=`:某个 agent 的审计要该 agent 的管理权,全站审计只给站点管理员。

## 从老版本迁过来

老 Hub 的 token 没有属主(一个 token = 整个 host 的通行证),映射不到新 ACL,因此**一律失效**;
`agit-hub token list` 会把它们标出来,重发即可。老仓库在 `agents.json` 里没有记录 → 按
**无主私有**对待(只有站点管理员看得见),用 `agit-hub add <name> --owner <user>` 认领。
两种都会在 `serve` 启动时打印提醒。

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

| 路径 | 内容 | 需要 |
|---|---|---|
| `GET /` 及任意网页路由 | React SPA 外壳(`/assets/app.js` + `app.css` 编译期嵌入) | — |
| `POST /api/login` · `POST /api/logout` · `GET /api/me` | 会话 | — |
| `GET /api/agents` | agent 花名册:名字、`aid`、session 数、最近活动、可见性、你的角色 | 只列你看得见的 |
| `POST /api/agents` | 建 agent(`{name, visibility}`,默认 private)→ `201 {name, aid, clone_url}` | 登录 |
| `GET /api/agent/<name>?page=&q=` | 会话摘要(脊线、provenance…)+ 历史 + `aid`/环境/分支/大小/runtime/成员 | 读 |
| `PATCH /api/agent/<name>` · `DELETE /api/agent/<name>` | 改名/改可见性 · 删库 | 管理 |
| `GET·POST /api/agent/<name>/members` · `DELETE .../members/<user>` | 成员表 | 读 · 管理 |
| `GET /api/agent/<name>/session/<id>?at=` | 单条 session 全貌 + revision 列表(`at=` pin 到历史提交) | 读 |
| `GET /api/agent/<name>/session/<id>/diff?from=&to=` | 两版的**语义** diff(指令/文件/结论增减,不是 jsonl 行噪声) | 读 |
| `GET·POST /api/tokens` · `DELETE /api/tokens/<id>` | token 自助(明文只回一次) | 登录会话 |
| `GET /api/audit?agent=&limit=` | 审计 | 管理 / 站点管理员 |
| `/<name>.git/...` | git smart-http(push/pull/clone) | 读 / push 要写 |

**`aid` 从哪来:** `agt_<uuid>` 由**客户端**铸造并提交在 store 里的 `agent.toml`,
Hub 只 `git show <ref>:agent.toml` 读它、不铸造。空库(刚建、还没人推)就老实报 `aid: null`
(`aid_source` 说明是 `none` 还是 `unidentified`)。改名不改身份:name 只是个可变标签。

**session 布局:** `sessions/<env>/<runtime>/<id>.jsonl` 与老的 `sessions/<runtime>/<id>.jsonl`
**都认**(老布局的 `env` 报 `null`)。claude-code 与 codex 是对等的 runtime,列表按字母序。

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
- **没有 TLS**:Hub 自己不终结 HTTPS,要么只听环回,要么前面挂反代(`--tls`)。
- **会话在内存里**:进程重启 = 全体重登(换来的是撤销立即生效)。token 不受影响。
- **单进程**:`users.json`/`agents.json`/`auth.json` 的读改写靠进程内锁 + 原子 rename;
  多个 `agit-hub serve` 指向**同一个 root** 会互相覆盖,不支持。
- **搜索上限**:带 `?q=` 时最多扫最近若干条 session(超出会在响应里标记,不静默截断)。
- Windows 下 `git http-backend` 行为不同,未验证。
