---
title: 自建 hub
parent: 中文文档
nav_order: 11
---

# 自建 hub

`agit-hub` 是一个自包含的 HTTP 服务，托管您团队的 agent 存储库（会话记录的裸 git 仓库）。人们通过内嵌的网页界面浏览它们，agent 则经由 git smart-http 推送和拉取它们。它自带认证（供人使用的 cookie 会话，供 git 和脚本使用的作用域令牌）、逐 agent 的访问检查、一份审计日志，以及每次推送时服务器端的密钥扫描。它的元数据存在一个数据库里（默认 SQLite，生产用 Postgres），大对象则存入一个按内容寻址的大对象存储（默认本地文件系统，配置后可用 S3/Garage）。

本指南涵盖两种受支持的运行方式（反向代理后的容器，以及反向代理后的 systemd 服务）及各项运维内容：TLS（为什么它是强制的）、可信代理的设置、数据库与大对象后端、注册，以及备份与升级。

下文的一切都使用**真实的** CLI。完整的子命令面为：

```
agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]
               [--tls] [--insecure] [--trusted-proxy IP,IP]      start the Hub
agit-hub user add <name> [--admin]                   add a user (asks for the password)
agit-hub user verify-email <name>                    force-mark a user's email verified (admin vouch)
agit-hub user verify-link <name>                     print a verification link to forward to the user
agit-hub user list                                   list users
agit-hub add <name> [--owner <user>] [--public] [--initialize]   new Agent Store (private by default)
agit-hub list                                        list hosted agents
agit-hub token add <name> [--user <owner>] [--agent <owner>/<name>]
                   [--read|--write] [--ttl-days N]   issue an access token
agit-hub token list                                  list tokens (metadata only)
agit-hub token rm <id>                               revoke a token
agit-hub org invite <org> <user> [--role R]          invite a user into an org (pending)
agit-hub org invitations <org>                       list an org's pending invitations
agit-hub org transfer <org> <new_owner>              hand org ownership to a member
agit-hub org rm <org>                                delete an empty org
agit-hub backup [--out <file.tgz>]                   one tar.gz: data root + a consistent metadata snapshot (0600, sensitive)
agit-hub restore <file.tgz> [--force]                inverse; refuses a non-empty root or a cross-backend restore
```

`agit-hub --help` 打印的正是这些。另有两个开关在 serve 时设置，不出现在上述摘要中：`--open-registration`（在[注册](#enabling-self-service-registration)一节说明），以及数据库和大对象后端 —— 它们由环境变量（`AGIT_HUB_DB`、`AGIT_HUB_S3_ENDPOINT`；参见[数据库与大对象后端](#the-database-and-blob-backends)）选择。组织通过 API 和网页界面创建；上面的 `org` 子命令管理一个已存在组织的邀请、所有权转移和删除。

---

## 您所部署进去的安全模型

有四条默认设定至关重要，部署时必须尊重它们，而非与之对抗：

1. **默认仅回环。** 不带 `--host` 时，hub 只绑定 `127.0.0.1:8177`。它持有您团队的全部会话记录历史；“安装它就把它暴露给办公室网络”不允许成为默认。

2. **它拒绝处于不安全状态。** 以明文绑定一个非回环地址会被直接拒绝（退出码 2）。要绑定到回环之外，您必须传入 **`--tls` 或 `--insecure` 之一**：

   ```
   $ agit-hub serve --host 0.0.0.0
   refusing to listen on 0.0.0.0 in plaintext.
   Other people on this address's network can reach it, and without TLS, login
   passwords and tokens cross the wire in plaintext ...
   ```

   `--tls` **并不**让 hub 说 TLS（hub 从不自己终结 TLS）。它是一个承诺：TLS 由前面的反向代理终结。它放松绑定守卫，并把会话 cookie 标记为 `Secure`。`--insecure` 则是留给可信局域网或一次性演示的、故意用明文的逃生口。

3. **磁盘上的机密被锁死。** 数据根以 `0700` 创建。在默认的 SQLite 后端上，元数据数据库（`hub.db` 及其预写日志附属文件）以 `0600` 写入；用 Postgres 时，同样的元数据改存于数据库中。密码以 argon2id 哈希存储，明文绝不落盘。令牌只以 sha256 摘要存储；令牌字符串在签发时展示一次。

4. **真实客户端 IP 来自 `--trusted-proxy`。** 在代理之后，hub 看到的对端是代理的 IP。它只从您在 `--trusted-proxy IP,IP` 中指名的对端读取 `X-Forwarded-For`。请在那里指名您的代理，否则逐 IP 的速率限制会以代理的地址为键，于是每个客户端共享同一个桶。

### 一旦离开回环，为什么 TLS 就是强制的

登录发送密码；git 和脚本发送令牌；服务器发回完整的会话记录。没有 TLS，这一切都以明文穿过线路，路径上的任何一跳都能复制一个令牌，进而读取或推送您团队的整部历史。这正是 hub 拒绝公开明文绑定、除非您强制它的原因。**永远在 hub 前面终结 HTTPS。** 下面两种拓扑都做到了这一点。

---

## 方案 A：Caddy 后的 Docker（推荐）

文件：[`Dockerfile`](../../Dockerfile)、[`.dockerignore`](../../.dockerignore)、[`deploy/docker-compose.yml`](../../deploy/docker-compose.yml)、[`deploy/Caddyfile`](../../deploy/Caddyfile)、[`deploy/garage.toml`](../../deploy/garage.toml)。

```
client ──HTTPS──▶ caddy (:443) ──HTTP──▶ hub (:8177)
```

Caddy 终结 HTTPS，并经由 Docker 内部网络把请求反向代理到 hub，同时转发 `X-Forwarded-For`。在容器内部，hub 以 `--tls`（TLS 在前面终结）绑定 `0.0.0.0:8177`，并信任 Caddy 的固定地址。除 Caddy 外，没有任何东西能触及 hub，因为 hub 不发布任何主机端口。

compose 文件捆绑了四个服务：**caddy**（前面的 TLS）、**hub**、**postgres**（生产元数据后端）和 **garage**（S3 兼容的大对象存储）。只有 Caddy 发布主机端口；postgres 和 garage 留在 compose 网络内部。Postgres 默认已接好；garage 则一直闲置，直到您把 hub 指向它。这两种后端接下来都会讲到。

### 镜像

`Dockerfile` 是多阶段的：

- **build** 阶段在 `rust:1-slim-bookworm` 上运行 `cargo build --release --bin agit-hub`。无需 Node：前端（`hub-ui/dist`）已提交，并在编译时嵌入二进制文件。
- **runtime** 阶段在 `debian:bookworm-slim` 上安装 `git`（hub 会外调它执行 receive-pack / rev-list / cat-file）和 `ca-certificates`，添加一个非 root 用户 `agithub`（uid 10001），并以它运行。数据根是位于 `/data` 的一个 `VOLUME`；由于 `HOME=/data`，hub 默认的 `--root` 无论对 `serve` 还是对每一条管理命令，都解析为 `/data/.agit-hub`。

默认的 `CMD` 是 `serve --host 0.0.0.0 --port 8177 --tls`，即绑定守卫自身指引的那个容器模型。`docker-compose.yml` 仅为添加 `--trusted-proxy` 而覆盖它。

### 启动

在仓库根目录下：

```sh
HUB_DOMAIN=hub.example.com docker compose -f deploy/docker-compose.yml up -d --build
```

先把 `HUB_DOMAIN` 的 DNS 指向这台主机；Caddy 会自动获取并续期一张 Let's Encrypt 证书。若只作本地试用，去掉 `HUB_DOMAIN`（它默认为 `localhost`，Caddy 会用自己本地受信的证书为其提供服务）；对于任何其他私有名称或裸 IP，请在 `Caddyfile` 中取消 `tls internal` 的注释。

### 数据库与大对象后端 {#the-database-and-blob-backends}

两个相互独立的选择藏在两个环境变量之后。两者都有零配置的默认值，因此一个两者都不设的 hub 会运行在 SQLite 和本地磁盘大对象之上。

**元数据（`AGIT_HUB_DB`）。** compose 文件默认把 hub 指向捆绑的 Postgres：

```yaml
AGIT_HUB_DB: postgres://agithub:${PGPASSWORD:-agithub}@postgres:5432/agithub
```

真实部署请设一个强 `PGPASSWORD`（它与 postgres 服务共享）。若想改回落到 `/data` 卷上零配置的 SQLite `hub.db`，去掉那一行即可。无论哪种方式，hub 都会在启动时自行创建并迁移它的表，因此没有初始化 SQL，而裸 git 仓库和 `audit.log` 仍然存放在 `/data` 上。

**大对象（`AGIT_HUB_S3_ENDPOINT`）。** 留空（默认）时，大对象存放在 `/data` 卷上的 `<root>/blobs` 下，garage 服务闲置。若想改存到 Garage，请在 hub 服务中取消 `AGIT_HUB_S3_*` 块的注释：

```yaml
AGIT_HUB_S3_ENDPOINT: http://garage:3900
AGIT_HUB_S3_BUCKET: agit-blobs
AGIT_HUB_S3_REGION: garage
AGIT_HUB_S3_ACCESS_KEY: ${GARAGE_ACCESS_KEY}
AGIT_HUB_S3_SECRET_KEY: ${GARAGE_SECRET_KEY}
```

Garage 不会自动创建它的布局、桶或密钥，因此以 Garage 为后端的部署在首次 `up` 之后需要一次性的初始化（很像创建第一个管理员）：

```sh
# 1. assign a storage layout to the single node (its id comes from `status`)
docker compose -f deploy/docker-compose.yml exec garage /garage status
docker compose -f deploy/docker-compose.yml exec garage /garage layout assign -z dc1 -c 1G <node-id>
docker compose -f deploy/docker-compose.yml exec garage /garage layout apply --version 1
# 2. create the bucket the hub will use
docker compose -f deploy/docker-compose.yml exec garage /garage bucket create agit-blobs
# 3. mint an access key (prints an Access Key ID + Secret, capture both)
docker compose -f deploy/docker-compose.yml exec garage /garage key create agit-hub-key
# 4. grant that key read+write on the bucket
docker compose -f deploy/docker-compose.yml exec garage /garage bucket allow --read --write agit-blobs --key agit-hub-key
```

然后带上环境中的密钥材料把 hub 启动起来：

```sh
GARAGE_ACCESS_KEY=<id> GARAGE_SECRET_KEY=<secret> \
  docker compose -f deploy/docker-compose.yml up -d
```

一个配置错误的 S3 端点（设置了，却缺少桶或密钥）会在启动时报错，而不会悄悄回退到本地磁盘。

### 第一个管理员、第一个 agent、第一个令牌

密码提示需要一个 TTY，因此运行 `exec` 时**不要**带 `-T`：

```sh
# 1. the first user must be a site admin
docker compose -f deploy/docker-compose.yml exec hub agit-hub user add you --admin
#    → prompts for a password (twice), stored as argon2id

# 2. a private Agent Store (private is the default; add --public to publish)
docker compose -f deploy/docker-compose.yml exec hub agit-hub add payments --owner you

# 3. a scoped, expiring write token for pushing to it
docker compose -f deploy/docker-compose.yml exec hub \
  agit-hub token add ci-writer --user you --agent payments --write --ttl-days 90
#    → prints the token ONCE. Copy it now; only its sha256 digest is stored.
```

这些 `exec` 命令都无需 `--root`：镜像中的 `$HOME=/data` 使每一条子命令都解析到同一个数据根。

从客户端核对并发布：

```sh
docker compose -f deploy/docker-compose.yml exec hub agit-hub user list
docker compose -f deploy/docker-compose.yml exec hub agit-hub list

# from a machine with the agit client and the token from step 3:
agit a remote add origin https://hub.example.com/payments.git
agit a push -u origin main
#   git prompts for a username/password: put the TOKEN in the password field
#   (the username can be anything).
```

用于拉取 agent 的只读凭据，就是同一条命令去掉 `--write`（读是默认）：`token add reader --user you --agent payments --read --ttl-days 90`。

---

## 方案 B：本地反向代理后的 systemd

文件：[`deploy/agit-hub.service`](../../deploy/agit-hub.service)。

用于非容器主机。hub 绑定**回环** `127.0.0.1:8177`（无需 `--tls`/`--insecure`），由*同一主机*上的反向代理终结 HTTPS 并转发给它。`--trusted-proxy 127.0.0.1` 让 hub 读取代理的 `X-Forwarded-For`。

### 安装

```sh
# 1. the binary
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub

# 2. the non-root service user (matches User= in the unit)
sudo useradd --system --home-dir /var/lib/agit-hub --shell /usr/sbin/nologin agithub

# 3. the unit (StateDirectory=agit-hub creates /var/lib/agit-hub 0700 on start)
sudo install -m 0644 deploy/agit-hub.service /etc/systemd/system/agit-hub.service
sudo systemctl daemon-reload
sudo systemctl enable --now agit-hub.service
sudo systemctl status agit-hub.service
```

该单元运行 `agit-hub serve --host 127.0.0.1 --port 8177 --root /var/lib/agit-hub --trusted-proxy 127.0.0.1`，失败时重启，并处于沙箱之中：`NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`PrivateDevices`、`ProtectKernel*`/`ProtectControlGroups` 一族、一个空的 `CapabilityBoundingSet`、`SystemCallFilter=@system-service`、`RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`，以及 `MemoryDenyWriteExecute`。部署前用 `systemd-analyze verify /etc/systemd/system/agit-hub.service` 核对它。

### 管理命令：留意 `--root`

服务的数据根是 `/var/lib/agit-hub`。手动的管理命令必须指向**同一个**根，并以服务用户身份运行，否则它们会读取/创建另一个空目录：

```sh
sudo -u agithub agit-hub user add you --admin       --root /var/lib/agit-hub
sudo -u agithub agit-hub add payments --owner you   --root /var/lib/agit-hub
sudo -u agithub agit-hub token add ci-writer --user you --agent payments \
                                     --write --ttl-days 90 --root /var/lib/agit-hub
```

### 前面的代理

任何终结 TLS 的代理都可以；它必须转发 `X-Forwarded-For`。一个最小的 nginx server 块：

```nginx
server {
    listen 443 ssl;
    server_name hub.example.com;

    ssl_certificate     /etc/letsencrypt/live/hub.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hub.example.com/privkey.pem;

    # git pushes and full transcripts can be large; do not cap the body.
    client_max_body_size 0;

    location / {
        proxy_pass         http://127.0.0.1:8177;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;
    }
}
```

`$proxy_add_x_forwarded_for` 会追加真实客户端 IP；hub 因信任 `127.0.0.1` 而把它读出来。如果您的代理位于另一台主机上，请把单元的绑定改到那个网络接口，加上 `--tls`，并把 `--trusted-proxy` 设为代理的地址。

---

## 启用自助注册 {#enabling-self-service-registration}

账户默认由站点管理员（`agit-hub user add`）创建：hub 是邀请制的。要让人们自行创建账户，请在 serve 时开启注册，用标志或环境变量皆可：

```sh
# flag, on the serve command
agit-hub serve ... --open-registration

# or the environment variable (1 / true / open / yes)
AGIT_HUB_REGISTRATION=1 agit-hub serve ...
```

在 compose 下，把 `--open-registration` 加进 hub 的 `command:` 列表，或在它的 `environment:` 中设置 `AGIT_HUB_REGISTRATION`；在 systemd 下，把该标志加进单元的 `ExecStart`。这会开放 `POST /api/register`，它创建一个**普通的、非管理员**账户并将其登录。注册永远无法授予管理员：那始终只限 CLI（`agit-hub user add --admin`）。启动横幅会报告当前模式（`signup: open` 或 `invite-only`）。

---

## 可信代理 / X-Forwarded-For，精确而言

hub 用请求的源 IP 做它逐 IP 的速率限制，如果它盲目信任 `X-Forwarded-For`，就极易被伪造。所以：

- **不设** `--trusted-proxy` 时，它完全忽略 `X-Forwarded-For`，以原始对端 IP 为键。
- 设了 `--trusted-proxy` 时，仅当对端是那些地址之一，它才从右向左遍历 `X-Forwarded-For`，取第一个不是可信代理的地址（即真实客户端）。一条格式错误或全部可信的链会回落到对端。

把 `--trusted-proxy` 设为连接到 hub 的那个代理的地址（方案 A 中 Caddy 固定的 `172.28.0.2`，方案 B 中的 `127.0.0.1`），除此之外别无其他。

---

## 备份

使用内置的 `agit-hub backup` 和 `agit-hub restore` 命令。它们读取与服务器相同的 `AGIT_HUB_DB` / `AGIT_HUB_S3_ENDPOINT`，因此一条命令就能捕获您所配置的任何后端。数据根在容器的 `hub-data` 卷内是 `/data/.agit-hub`，在 systemd 下则是 `/var/lib/agit-hub`。

```sh
# Take a backup (one 0600 tar.gz; defaults to ./agit-hub-backup-<timestamp>.tgz):
agit-hub backup --root /var/lib/agit-hub --out /secure/agit-hub-$(date +%F).tgz

# Restore it into a data root (refuses a non-empty root without --force):
agit-hub restore /secure/agit-hub-2026-01-31.tgz --root /var/lib/agit-hub
```

运行 `restore` 时要停掉 hub，并设好与目标 hub 所用**相同**的 `AGIT_HUB_DB`：该命令会把元数据后端记录在归档中，并拒绝跨后端还原（把 SQLite 转储还原进 Postgres 目标，反之亦然）。对 Postgres，它会还原进 `AGIT_HUB_DB` 所指的数据库，因此那个变量必须设置。

压缩包里包含什么：

- **裸仓库：** 数据根下的 `<owner>/<name>.git/`，每个 agent 一个，即真实的会话记录历史。
- **`audit.log`：** 只追加的审计轨迹，同样在数据根下。
- **元数据数据库：** 在默认的 SQLite 后端上，是一份一致的 `hub.db` 快照（用 SQLite 在线的 `VACUUM INTO` 获取，绝不是对活动 WAL 文件的原始复制）；在 Postgres 后端上，则是一份 `pg_dump`（`metadata.sql`）。它含有密码哈希和令牌摘要，因此整个压缩包以 `0600` 写入，即便那些摘要并非可逆的明文，它也是敏感的。请把它存在主机之外。
- **大对象：** 在文件系统后端上，`<root>/blobs` 在压缩包内。在 **S3/Garage 后端上，大对象是外部的**，**不**在压缩包内：`backup` 会大声告警，并在归档的 `manifest.json` 中记录 `external_blobs: true`。请单独备份 Garage 自己的存储（compose 下的 `garage-meta` 和 `garage-data` 卷）。

临时文件（`hub.db-wal`/`hub.db-shm` 附属文件，已折入快照，以及任何 `*.lock`）被排除，`restore` 在提取前会对每一个归档成员防范路径穿越。

在底层，如果您需要手动执行，步骤如下：为得到一致的副本先停掉 hub，然后使用每种后端自己的工具（Postgres 用 `pg_dump`，SQLite 用 `.backup`/`VACUUM INTO`），再对数据根做一次 `tar`。上述命令的 Postgres 路径需要 `pg_dump`/`psql` 在 `PATH` 上。对于容器化部署，您也可以用一个一次性容器快照数据根卷（如果您使用那些后端，对 `pg-data` 和 garage 各卷同样处理）：

```sh
docker run --rm -v deploy_hub-data:/data -v "$PWD":/backup debian:bookworm-slim \
  tar czf /backup/agit-hub-$(date +%F).tgz -C /data .
```

（`deploy_hub-data` 是 compose 加前缀后的卷名；用 `docker volume ls` 确认。）还原时，在 hub 停止的情况下把同一个压缩包解压回该卷即可。

---

## 升级

二进制文件是自包含的（前端已编译进去），且 hub 在启动时运行它自己的数据库迁移（幂等，由一行 schema 版本记录把关），因此升级就是替换并重启，无需手动迁移：

**Docker：** 重新构建并滚动 hub；卷会把数据带过去。

```sh
git pull
HUB_DOMAIN=hub.example.com docker compose -f deploy/docker-compose.yml up -d --build
```

**systemd：** 备份正在运行的二进制文件，放入新的，然后重启。保留旧的二进制文件就是您的回滚方案：如果新的起不来，一次 `install` 恢复备份再重启，就回到原状。

```sh
sudo cp -p /usr/local/bin/agit-hub /usr/local/bin/agit-hub.bak   # rollback point
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub
sudo systemctl restart agit-hub.service
```

迁移在一个事务中运行，且失败即关闭：如果某个迁移无法完成，启动会中止，没有任何 ref 或行被写到一半，因此旧的二进制文件能对未改动的数据干净地启动。重启后 `systemctl is-active agit-hub` 就是检验迁移是否走通的方法。

您从不手动运行迁移。启动时 hub 会报告任何需要注意的事项：没有账户的用户、待认领的旧的无主仓库（`agit-hub add <name> --owner <user>`），以及没有所有者的旧令牌（在当前 ACL 下已失效，因此用 `agit-hub token add … --user <owner>` 重新签发，并用 `agit-hub token rm <id>` 丢弃旧的）。

任何升级前都先备份数据根。
