---
sidebar_position: 4
title: 运维
---

# 运维

中枢的日常运维：诊断命令、版本与指标端点、将客户端错误与服务端日志行绑定起来的
请求关联 id，以及如何读懂你实际会遇到的两类拒绝（被拒的推送与失败的登录）。

## `agit-hub doctor`

运维者诊断工具。它会报告一次 `serve` 将以何种配置启动，读取环境中相同的
`AGIT_HUB_DB` 与 `AGIT_HUB_S3_*`，并探测各后端是否可达：

```sh
sudo -u agithub agit-hub doctor --root /var/lib/agit-hub
```

报告分为五个部分：

- **VERSION**：中枢版本、构建 sha（编译时嵌入的情况下）以及模式版本。
- **DATABASE**：后端（`sqlite`/`postgres`）、目标（仅主机与数据库名，绝不含凭据）、
  各表行数，以及一行健康状态。行数兼作可达性探测：一张能应答的表就是一张中枢能触及
  的表。
- **BLOB STORAGE**：后端，S3 情况下还包括端点、bucket 与 region，访问密钥与密钥显示
  为 `set (masked)`，绝不会读入报告。可达性探测会对一个格式正确、不存在的对象发起
  HEAD 请求。
- **DATA ROOT**：路径、磁盘占用大小以及可用空间。
- **CONFIG**：注册模式与监听形态。

凭据被两次脱敏：显式的字段掩码会去掉数据库的 userinfo 并隐藏 S3 密钥，最后一遍还会
把整份报告过一遍密钥扫描器，并对它标记的任何行进行掩码。非零退出码意味着某个后端
无法打开，这本身就是最重要的诊断信息。`agit-hub doctor` 是客户端
[诊断](../cli/diagnostics.md)在服务端的对应工具。

## `GET /api/version`

公开，无需认证。返回版本、构建 sha（编译时未设置时返回 `null`，绝不返回伪造值）以及
模式版本：

```sh
curl -s https://hub.example.com/api/version
```

```json
{"version":"0.2.1","build_sha":"…","schema_version":37}
```

可用它作为从外部进行的正常运行（uptime）与版本检查，也可用它确认客户端实际对话的是
哪个版本。

## `GET /metrics`

Prometheus 文本呈现格式，**受管理员权限限制**。它经由与其他所有路由相同的认证提供，
非管理员（或匿名）调用者会得到与不存在的路由相同的 `404`，因此没有管理员凭据时
`/metrics` 甚至无法被发现。用密码字段中的管理员令牌来抓取它：

```sh
curl -s -u x:$ADMIN_TOKEN https://hub.example.com/metrics
```

它暴露的时间序列：

| 指标 | 类型 | 含义 |
| --- | --- | --- |
| `agit_hub_build_info{version}` | gauge | 恒为 1；携带版本标签。 |
| `agit_hub_uptime_seconds` | gauge | 自该进程开始服务以来的秒数。 |
| `http_requests_total{method,status}` | counter | 按方法与状态类别统计的请求数。 |
| `http_request_duration_seconds` | histogram | 请求延迟。 |
| `auth_attempts_total{result}` | counter | 按结果统计的认证尝试数。 |
| `git_push_total{result}` | counter | 推送尝试，`accepted` 对 `rejected`。 |
| `secret_scan_rejects_total` | counter | 被密钥扫描在进程内拒绝的推送数。 |

像签发任何其他管理员令牌一样签发这个抓取凭据
（`agit-hub token add --user <admin>`）；参见[令牌](../hub/tokens.md)。

## X-Request-Id 关联

每个响应都携带一个 `X-Request-Id` 头，这是一个由服务端为每个请求生成一次的 16 位
十六进制 id。调用方提供的 `X-Request-Id` 会被有意忽略，因此客户端无法伪造或制造
id 冲突。对于 JSON 错误响应体（状态码 >= 400），同一个 id 会作为 `request_id` 字段
折叠进该对象，以便客户端与 Web 界面能够呈现它。

该 id 也会标记该请求的结构化日志行（`request_id=…`）。当用户报告一个错误时，向其索取
错误中的 `request_id` 并在日志中 grep 它：它把客户端可见的失败与确切的服务端日志行
绑定起来，其中就包括下文所述的认证与推送决策。这正是[报告问题](../hub/reporting-problems.md)
要求用户附上的信息。

## 读懂一次被拒的推送

一次未授权的推送（receive-pack）会在其 pack 被读入内存之前，就在授权关卡处被拒绝。
有三处会记录它：

- 一条带有行为者、受限智能体与动作的**审计日志**拒绝条目；
- 一行**结构化日志**，`git push rejected`，携带 `agent`、`actor` 与 `reason`；
- **指标** `git_push_total{result="rejected"}`。

客户端会看到一个带 `WWW-Authenticate` 头的 `401`，因为 git 客户端只在收到 401 时才
提示输入凭据。因此一次不断重复提示输入密码的推送，几乎总是 ACL 拒绝，而非密码错误：
请检查令牌的作用域与智能体的可见性。一次已授权的推送会使
`git_push_total{result="accepted"}` 递增并记录 `git push accepted`；随后服务端密钥
扫描会在进程外的 pre-receive 钩子中运行，一次扫描拒绝在审计日志中具有权威性。

## 读懂一次失败的认证

一次失败的登录会记录一条 `login failed` 警告（带规范化后的用户名，绝不带密码），
写入一条 `LOGIN_FAILED` 审计条目，使 `auth_attempts_total{result="login_fail"}`
递增，并返回一个笼统的 `401 wrong username or password`。无论是用户不存在还是密码
错误，该消息都是有意保持一致的，因此它不会给暴力破解者递上任何用户名字典。

一个被出示但解析不到任何对象（已过期、已吊销或未知）的令牌会记录 `token denied`，
不含头部内容，并使被拒认证计数器递增。一个超出其按令牌请求预算的令牌会得到一个带
`Retry-After` 的 `429`；该预算是按令牌而非按地址计的，因此一个吵闹的令牌无法耗尽
所有人的配额。Argon2 有意设计得很慢，因此登录受并发限制；一波登录会排队，而不会
把每个核心都跑满。

若认证在全中枢范围内失败，先从 `agit-hub doctor` 开始（数据库可达吗？），再看
`GET /api/version`（进程在服务吗？），然后在日志中查该请求的 `X-Request-Id`。
