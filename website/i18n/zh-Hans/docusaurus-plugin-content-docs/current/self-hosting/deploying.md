---
sidebar_position: 1
title: 部署中枢
---

# 部署中枢

`agit-hub` 是一个自包含的 HTTP 可执行文件。它托管你团队的智能体存储库（agent
stores，即存放会话转录记录的裸 git 仓库），提供用于浏览这些记录的 Web 界面，并
以 git smart-http 协议响应 CLI 的推送（push）与拉取（pull）。它内置了身份认证、
按智能体维度的访问检查、审计日志，以及在每次推送时执行的服务端密钥扫描（secret
scan）。前端已编译进该可执行文件，因此无需另行提供静态资源服务。

本页介绍如何搭建一套中枢：存储后端、在 docker compose 或 systemd 下运行它，以及
在其前方部署终止 TLS 的反向代理。关于它读取的环境变量，参见
[配置](./configuration.md)。关于日常运维，参见[运维](./operations.md)。

## 各部分运行在何处

中枢将其状态保存在两处：

- **元数据**（用户、智能体、令牌、ACL、审计记录）保存在数据库中。默认使用 SQLite，
  生产环境使用 Postgres，由 `AGIT_HUB_DB` 选择。
- **Blob**（大型内容寻址对象）保存在 blob 存储中。默认使用本地文件系统，配置后可
  使用 Garage 或任意兼容 S3 的存储，由 `AGIT_HUB_S3_ENDPOINT` 选择。

两者都有零配置默认值，因此未设置任何变量的中枢会在其数据根目录下以 SQLite 与本地
磁盘 blob 运行。通过在[配置](./configuration.md)中设置相应变量即可迁移到 Postgres
与 S3；中枢在启动时会自行创建并迁移其表结构，因此无需任何初始化 SQL。

## 你所部署进入的安全防线

有四项默认设置起着关键的承载作用。

1. **默认仅监听回环地址。** 未指定 `--host` 时，中枢仅绑定 `127.0.0.1:8177`。它保存
   着你团队的全部转录历史，因此将其暴露到网络绝不会是默认行为。

2. **它会拒绝以不安全方式运行。** 以明文绑定非回环地址会以退出码 2 退出：

   ```
   $ agit-hub serve --host 0.0.0.0
   refusing to listen on 0.0.0.0 in plaintext.
   Other people on this address's network can reach it — and without TLS, login passwords and
   tokens cross the wire in plaintext ...
   ```

   若要绑定回环地址以外的地址，须传入 **`--tls` 或 `--insecure` 二者之一**。`--tls`
   并不会让中枢自己讲 TLS（它从不自行终止 TLS）。它是一项承诺：由前方的反向代理终止
   TLS；它会放宽绑定防线，并将会话 cookie 标记为 `Secure`。`--insecure` 则是面向可信
   局域网或临时演示的、有意为之的明文逃生出口。

3. **磁盘上的机密受到严格锁定。** 数据根目录以 `0700` 创建。在 SQLite 下，元数据库
   （`hub.db` 及其 WAL 附属文件）以 `0600` 写入；使用 Postgres 时，该元数据改为存放
   在数据库中。密码以 argon2id 哈希存储，令牌以 sha256 摘要存储，因此两者的明文都不
   会落到磁盘上。

4. **真实客户端 IP 来自 `--trusted-proxy`。** 在代理之后，中枢看到的对端地址是代理的
   地址。它只从你在 `--trusted-proxy IP,IP` 中指定的对端读取 `X-Forwarded-For`。请在
   那里指定你的代理，否则按 IP 限流会以代理地址为键，导致所有客户端共用同一个限流桶。

:::danger
登录会发送密码，git 与脚本会发送令牌，服务端会回传完整的会话转录记录。若无 TLS，
这一切都会以明文形式在网络上传输。一旦离开回环地址，务必始终在中枢前方终止 HTTPS。
:::

## 方案 A：docker compose 加反向代理

代码仓库在 `deploy/` 下附带了一个 compose 文件，捆绑了四个服务：反向代理（前方终止
TLS）、**hub**、**postgres**（生产元数据后端）以及 **garage**（兼容 S3 的 blob
存储）。只有代理会对外发布主机端口；postgres 与 garage 保持在 compose 网络内部。

```
client ──HTTPS──▶ proxy (:443) ──HTTP──▶ hub (:8177)
```

在容器内部，中枢以 `--tls`（TLS 在前方终止）绑定 `0.0.0.0:8177`，并通过
`--trusted-proxy` 信任代理的固定地址。镜像以非 root 用户运行；其 `HOME` 即数据卷，
因此默认的 `--root` 对 `serve` 与每一条管理命令都解析到同一位置。

从仓库根目录启动，并先将 DNS 指向该主机，以便代理能够获取证书：

```sh
HUB_DOMAIN=agit.anggita.org docker compose -f deploy/docker-compose.yml up -d --build
```

Postgres 默认已接好线路。真实部署时请设置一个强 `PGPASSWORD`。Garage 会保持空闲，
直到你将中枢指向它；若要在其中存储 blob，请设置 `AGIT_HUB_S3_*` 一组变量并运行
Garage 的一次性初始化（layout、bucket、key）。两者都在[配置](./configuration.md)
中说明。

创建第一个管理员、一个私有智能体存储库以及一个写入令牌。密码提示需要 TTY，因此运行
`exec` 时不要加 `-T`：

```sh
# 第一个用户必须是站点管理员
docker compose -f deploy/docker-compose.yml exec hub agit-hub user add you --admin

# 一个私有智能体存储库（默认即私有；用 --public 发布）
docker compose -f deploy/docker-compose.yml exec hub agit-hub add payments --owner you

# 一个受限、会过期的写入令牌（仅打印一次；仅存储其 sha256 摘要）
docker compose -f deploy/docker-compose.yml exec hub \
  agit-hub token add ci-writer --user you --agent payments --write --ttl-days 90
```

这些 `exec` 命令无需 `--root`：容器的 `HOME` 使每个子命令都解析到同一数据根目录。
账户、令牌与智能体的相关内容参见[账户](../hub/accounts.md)与[令牌](../hub/tokens.md)。

## 方案 B：systemd 加本机反向代理

适用于非容器主机。中枢绑定回环地址 `127.0.0.1:8177`（无需 `--tls`/`--insecure`），
由同一主机上的反向代理终止 HTTPS 并转发到它。`--trusted-proxy 127.0.0.1` 让中枢得以
读取代理的 `X-Forwarded-For`。

安装可执行文件、一个非 root 服务用户，以及来自 `deploy/` 的 unit 文件：

```sh
# 1. 可执行文件
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub

# 2. 服务用户（与 unit 中的 User= 一致）
sudo useradd --system --home-dir /var/lib/agit-hub --shell /usr/sbin/nologin agithub

# 3. unit 文件（StateDirectory=agit-hub 以 0700 创建 /var/lib/agit-hub）
sudo install -m 0644 deploy/agit-hub.service /etc/systemd/system/agit-hub.service
sudo systemctl daemon-reload
sudo systemctl enable --now agit-hub.service
```

该 unit 运行 `agit-hub serve --host 127.0.0.1 --port 8177 --root /var/lib/agit-hub
--trusted-proxy 127.0.0.1`，在失败时重启，并处于沙箱之中（`NoNewPrivileges`、
`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、一个空的 `CapabilityBoundingSet`、
`SystemCallFilter=@system-service` 等等）。部署前先验证它：

```sh
systemd-analyze verify /etc/systemd/system/agit-hub.service
```

:::caution 注意管理命令上的 `--root`
服务数据根目录是 `/var/lib/agit-hub`。手动运行的管理命令必须指向同一根目录并以服务
用户身份运行，否则它们会读取或创建一个不同的空目录：

```sh
sudo -u agithub agit-hub user add you --admin --root /var/lib/agit-hub
sudo -u agithub agit-hub add payments --owner you --root /var/lib/agit-hub
```
:::

## 反向代理与 TLS

任何终止 TLS 的代理都可以。它必须转发 `X-Forwarded-For`，且不得对请求体设上限或做
缓冲：git 推送与完整转录记录体积很大，而 smart-http 是一种流式协议。一个最小化的
nginx server 块：

```nginx
server {
    listen 443 ssl;
    server_name agit.anggita.org;

    ssl_certificate     /etc/letsencrypt/live/agit.anggita.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/agit.anggita.org/privkey.pem;

    # git 推送与完整转录记录可能很大；不要对请求体设上限。
    client_max_body_size 0;

    location / {
        proxy_pass         http://127.0.0.1:8177;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;

        # smart-http 是流式的；不要先把它缓冲成文件。
        proxy_request_buffering off;
        proxy_buffering        off;
        proxy_http_version     1.1;
    }
}
```

`$proxy_add_x_forwarded_for` 会追加真实客户端 IP，中枢因信任 `127.0.0.1` 而将其读取
出来。若代理位于另一台主机，请把 unit 的绑定改为那个网络接口、加上 `--tls`，并将
`--trusted-proxy` 设为代理的地址。

为使基于密钥的认证在跨中枢时具备防重放能力，请将 `AGIT_HUB_PUBLIC_URL` 设为该代理的
公开源（`https://agit.anggita.org`）。原因参见[配置](./configuration.md)。

## 升级

该可执行文件是自包含的，并在启动时运行自己的数据库迁移（幂等，由一行模式版本记录
把关），因此升级即替换并重启，无需手动迁移。

**Docker：** 重新构建并滚动更新中枢；数据卷会带着数据跨越升级。

```sh
git pull
HUB_DOMAIN=agit.anggita.org docker compose -f deploy/docker-compose.yml up -d --build
```

**systemd：** 保留上一个可执行文件作为回滚点，放入新的可执行文件，然后重启。

```sh
sudo cp -p /usr/local/bin/agit-hub /usr/local/bin/agit-hub.bak   # 回滚点
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub
sudo systemctl restart agit-hub.service
```

迁移在事务内运行并采用失败即关闭（fail closed）策略：若某次迁移无法完成，则启动中止
且不会写入到一半状态，于是上一个可执行文件可以对未改动的数据干净地启动。重启后执行
`systemctl is-active agit-hub` 可确认它已起来。任何升级前请先备份数据根目录，参见
[备份与恢复](./backup-restore.md)。
