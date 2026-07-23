---
sidebar_position: 3
title: 备份与恢复
---

# 备份与恢复

`agit-hub backup` 会将中枢的持久状态写成单个 tarball；`agit-hub restore` 将其还原
回去。两者读取的 `AGIT_HUB_DB` 与 `AGIT_HUB_S3_ENDPOINT` 与服务端相同，因此一条命令
即可捕获你所配置的任意后端。你无需手动运行 `pg_dump` 和 `tar`。

```
agit-hub backup  [--out <file.tgz>] [--root <dir>]
agit-hub restore <file.tgz> [--root <dir>] [--force]
```

## 执行备份

```sh
# systemd 主机：指向服务根目录；默认文件名带时间戳，位于当前工作目录
sudo -u agithub agit-hub backup --root /var/lib/agit-hub \
  --out /secure/agit-hub-$(date +%F).tgz
```

未指定 `--out` 时，文件为当前目录下的 `agit-hub-backup-<timestamp>.tgz`，因此重复
运行绝不会相互覆盖。该命令会打印它所记录的后端、模式版本以及 blob 的存放位置：

```
wrote backup: /secure/agit-hub-2026-07-16.tgz
  backend:    postgres (schema v37)
  blobs:      s3 (EXTERNAL, not in this tarball)
  created:    2026-07-16T09:12:44Z
  ⚠ This file holds the metadata DB (password + token digests). It is 0600; keep it secret and off-host.
```

## tarball 包含什么

数据根目录的内容位于顶层（与手动执行 `tar czf ... -C /data .` 产生的形态相同，因此
你可以直接从中读取仓库），外加一个供恢复流程消费的、保留的 `.agit-backup/` 目录：

```text
./<owner>/<name>.git/…         裸仓库，每个智能体一个 —— 即转录历史
./audit.log                    仅追加的审计记录
./blobs/…                      文件系统 blob 存储（S3 后端下不存在）
.agit-backup/manifest.json     后端种类、模式版本、时间戳、外部 blob 标志
.agit-backup/hub.db            SQLite：一致的 VACUUM INTO 快照（绝非原始 WAL 拷贝）
.agit-backup/metadata.sql      Postgres：以 pg_dump 取代 hub.db
```

在 SQLite 下，元数据是用 SQLite 的在线 `VACUUM INTO` 获取的一致 `hub.db` 快照，绝非
对活动 WAL 文件的原始拷贝。在 Postgres 下则是一个 `pg_dump`（`metadata.sql`）来取代
它。临时文件（活动的 `hub.db-wal`/`hub.db-shm` 附属文件——它们已被折叠进快照——以及
任何 `*.lock`）都被排除在外。

该快照包含密码哈希与令牌摘要。整个 tarball 以 `0600` 写入，即便这些摘要并非可逆的
明文，它仍是敏感的。请将其保存在主机之外。

:::caution S3/Garage blob 是外部的
在 S3/Garage blob 后端下，blob 不在数据根目录中，因此它们**不在** tarball 内。
`backup` 会大声警告，并在清单中记录 `external_blobs: true`。请单独备份 Garage 自身
的存储（compose 下其 `garage-meta` 与 `garage-data` 卷，或你的 S3 提供商自身的快照
机制）。在文件系统 blob 后端下，`<root>/blobs` 就在 tarball 内部，无需单独步骤。
:::

## 恢复

在中枢已停止的状态下运行 `restore`，并设置与目标中枢所用相同的 `AGIT_HUB_DB`：

```sh
sudo systemctl stop agit-hub
sudo -u agithub agit-hub restore /secure/agit-hub-2026-07-16.tgz --root /var/lib/agit-hub
sudo systemctl start agit-hub
```

两道防护会拒绝踩坑操作：

- **非空根目录。** `restore` 会拒绝写入一个已含数据的数据根目录，因此绝不会静默覆盖
  一个运行中的中枢。传入 `--force` 可有意覆盖它。
- **跨后端。** 清单记录了快照来源的元数据后端，`restore` 会拒绝跨越它：将 SQLite
  转储恢复到 Postgres 目标（或反过来）都会被拒绝。对于 Postgres 备份，它会恢复到
  `AGIT_HUB_DB` 所指向的数据库，因此该变量必须设为目标数据库。

每个归档成员在解压前都会针对路径穿越（path traversal）进行检查。在 SQLite 下，恢复
后的行数会与清单在备份时记录的行数进行核对，因此一个被截断的短快照会被大声捕获，
而不会恢复出一个近乎空白的数据库。

## 调用中的坑

- **`--root` 必须与运行中的中枢一致。** 在 systemd 下服务根目录是
  `/var/lib/agit-hub`；在 docker 下则是数据卷。将 `backup`/`restore` 指向同一根目录，
  否则你快照或覆盖的是一个空目录。
- **`pg_dump` 与 `psql` 必须在 `PATH` 上**，两条命令的 Postgres 路径都需要它们。
- **也要为管理命令设置后端变量。** `backup` 与 `restore` 完全像 `serve` 一样，从
  `AGIT_HUB_DB` 与 `AGIT_HUB_S3_ENDPOINT` 挑选后端。请在携带这些变量的同一环境中
  （或在携带这些变量的同一 systemd/compose 服务下）运行它们。参见
  [配置](./configuration.md)。
- **恢复时中枢须处于停止状态。** 运行中的中枢持有模式写锁，在其之下恢复会与运行中的
  进程发生竞争。

对于容器化部署，你也可以用一个临时容器直接快照数据根目录卷，并连同其旁边的 Postgres
与 Garage 卷一起快照，但内置命令才是受支持的路径。
