---
sidebar_position: 2
title: 配置
---

# 配置

中枢通过 `agit-hub serve` 的命令行标志以及环境变量进行配置。标志设定监听形态与安全
姿态；环境变量选择存储后端以及若干行为开关。中枢不读取任何配置文件。凡是你为
`serve` 设置的，都要为 `agit-hub doctor`、`backup` 与 `restore` 设置相同的值，这样
管理命令便会连接到与服务端相同的后端。

## Serve 标志

```
agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]
               [--tls] [--insecure] [--trusted-proxy IP,IP]
               [--open-registration] [--public-url URL]
```

| 标志 | 默认值 | 作用 |
| --- | --- | --- |
| `--host` | `127.0.0.1` | 绑定的网络接口。回环地址默认将团队的历史记录挡在网络之外。 |
| `--port` | `8177` | 监听端口。 |
| `--root` | `$HOME/.agit-hub` | 数据根目录：裸仓库、`audit.log`、SQLite `hub.db`、文件系统 blob。 |
| `--tls` | 关闭 | 承诺由前方的代理终止 TLS。放宽明文绑定防线，并将 cookie 标记为 `Secure`。它并不会让中枢自己讲 TLS。 |
| `--insecure` | 关闭 | 有意在回环地址以外以明文监听。 |
| `--trusted-proxy` | 无 | 以逗号分隔的代理 IP，中枢信任它们的 `X-Forwarded-For` 以确定客户端地址。 |
| `--open-registration` | 关闭 | 启用自助注册（`POST /api/register`）。 |
| `--public-url` | 无 | 本中枢的规范基址 URL；用于固定密钥认证的受众（audience）。等同于 `AGIT_HUB_PUBLIC_URL`。 |

`--tls`/`--insecure` 与 `--trusted-proxy` 在[部署](./deploying.md)中有详细说明。

## 元数据后端：`AGIT_HUB_DB`

选择用户、智能体、令牌与 ACL 元数据的存放位置。

- **未设置，或任意非 URL 值：** 使用数据根目录下的 SQLite `hub.db`，以 `0600` 写入。
  零配置；适合单主机或试用。
- **一个 `postgres://` 或 `postgresql://` URL：** 使用 Postgres，即生产后端。

```sh
AGIT_HUB_DB=postgres://agithub:STRONGPASS@postgres:5432/agithub
```

中枢在启动时会自行创建并迁移其表结构，因此没有初始化 SQL。一个错误的 URL 或一个
不可达的 Postgres 会在启动时（而非首个请求时）明确报错。

## Blob 后端：`AGIT_HUB_S3_*` 一组变量

选择大型内容寻址对象的存放位置。若不设置，blob 会存储在文件系统 `<root>/blobs`
下。若要将其存储在 Garage 或任意兼容 S3 的存储中，请设置 `AGIT_HUB_S3_ENDPOINT`
（非空），这会开启 S3 后端并使其余变量成为必需：

| 变量 | 使用 S3 时是否必需 | 默认值 |
| --- | --- | --- |
| `AGIT_HUB_S3_ENDPOINT` | 用于选择 S3 | 未设置时使用文件系统后端 |
| `AGIT_HUB_S3_BUCKET` | 是 | 无 |
| `AGIT_HUB_S3_ACCESS_KEY` | 是 | 无 |
| `AGIT_HUB_S3_SECRET_KEY` | 是 | 无 |
| `AGIT_HUB_S3_REGION` | 否 | `garage` |

路径式寻址（path-style addressing）始终开启（Garage 要求如此）。

:::caution 启动时失败即关闭
若设置了 `AGIT_HUB_S3_ENDPOINT` 但 bucket 或某个 key 缺失或为空，中枢会在启动时报错。
它绝不会静默回退到本地磁盘，因此一个配置错误的端点绝不会悄悄把 blob 写到错误的位置。
:::

Garage 不会自动创建它的 layout、bucket 或 key。一套以 Garage 为后端的部署，在首次
`up` 之后需要一次性初始化：为节点分配存储 layout、创建 bucket、生成访问密钥并授予其
读写权限。用 `docker compose ... exec garage /garage ...` 运行这些操作，然后将密钥
材料放入 `AGIT_HUB_S3_ACCESS_KEY` / `AGIT_HUB_S3_SECRET_KEY` 并启动中枢。

## `AGIT_HUB_PUBLIC_URL`：固定密钥认证的受众

将其设为中枢自身的规范基址 URL（`scheme://host[:port]`，不含路径），例如
`https://agit.anggita.org`。`--public-url` 标志与之等价；末尾的斜杠会被去除。

基于密钥的认证会为某个特定中枢签署一段质询（challenge）。位于 `POST /api/auth/key`
的处理程序会依据此受众核对签名断言。由运维者配置的值受服务端控制，无法被请求头伪造，
因此为本中枢捕获的签名绝不能被重放到另一个中枢。当 `AGIT_HUB_PUBLIC_URL` 未设置时，
处理程序会回退到请求的 `Host` 头，而这仅是尽力而为（best-effort）。

:::danger 凡是可通过多个名称访问的部署都要设置此项
在反向代理之后，中枢无法得知自己的公开源（origin）。若不设置 `AGIT_HUB_PUBLIC_URL`，
能够操纵客户端请求 `Host`（或运行第二个中枢）的攻击者便有机会将密钥认证断言跨中枢
重放。请将其固定为你的代理所服务的确切公开源。密钥认证的客户端一侧参见
[身份认证](../integration/authentication.md)。
:::

## `AGIT_HUB_REGISTRATION`：自助注册

默认情况下账户由站点管理员创建（`agit-hub user add`）；中枢采用邀请制。若要允许人们
自行创建账户，请将 `AGIT_HUB_REGISTRATION` 设为 `1`、`true`、`open` 或 `yes`（或传入
`--open-registration`）。这会开启 `POST /api/register`，它会创建一个普通的、非管理员
账户并将其登录。注册永远不能授予管理员权限；管理员权限仅限 CLI。启动横幅会报告当前
模式（`signup: open` 或 `invite-only`）。

## 其他环境变量

| 变量 | 用途 | 默认值 |
| --- | --- | --- |
| `AGIT_HUB_BASE_URL` | 作为前缀加在中枢发出的邮箱验证与密码重置链接前的基址 URL。未设置时发出一个裸路径，交由运维者自行加前缀。 | 空（裸路径） |
| `AGIT_HUB_PROVENANCE_ENFORCE` | `1`/`true`/`yes`/`on` 会拒绝来源认证（provenance）未通过验证的推送。缺省/空白/false 时仅记录该发现。 | 仅记录 |
| `AGIT_HUB_LOG` | `tracing` 过滤指令，例如 `agit_hub=debug,info`。 | `info` |
| `AGIT_HUB_LOG_FORMAT` | `pretty`（人类可读）或 `json`（每行一个对象，供日志管道使用）。 | `pretty` |

`AGIT_HUB_BASE_URL` 设定[账户](../hub/accounts.md)邮件流程的链接基址；
`AGIT_HUB_PROVENANCE_ENFORCE` 把关[来源认证](../integration/provenance.md)一节所述的
推送时检查。两个日志变量在启动时读取一次；日志输出到 stderr，因此 stdout 上的启动
横幅保持干净。
