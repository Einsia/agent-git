---
sidebar_position: 3
title: Backup and restore
---

# Backup and restore

`agit-hub backup` writes one tarball of the hub's durable state; `agit-hub restore` puts
it back. Both read the same `AGIT_HUB_DB` and `AGIT_HUB_S3_ENDPOINT` the server does, so
one command captures whichever backends you configured. You never hand-run `pg_dump` and
`tar`.

```
agit-hub backup  [--out <file.tgz>] [--root <dir>]
agit-hub restore <file.tgz> [--root <dir>] [--force]
```

## Taking a backup

```sh
# systemd host: point at the service root; default name is timestamped in the cwd
sudo -u agithub agit-hub backup --root /var/lib/agit-hub \
  --out /secure/agit-hub-$(date +%F).tgz
```

With no `--out`, the file is `agit-hub-backup-<timestamp>.tgz` in the current directory,
so repeat runs never clobber each other. The command prints the backend, schema version,
and blob placement it recorded:

```
wrote backup: /secure/agit-hub-2026-07-16.tgz
  backend:    postgres (schema v37)
  blobs:      s3 (EXTERNAL, not in this tarball)
  created:    2026-07-16T09:12:44Z
  ⚠ This file holds the metadata DB (password + token digests). It is 0600; keep it secret and off-host.
```

## What the tarball contains

The data-root contents sit at the top level (the same shape a manual `tar czf ... -C
/data .` produces, so you can read repos straight out of it), plus one reserved
`.agit-backup/` directory the restore consumes:

```text
./<owner>/<name>.git/…         the bare repos, one per agent — the transcript history
./audit.log                    the append-only audit trail
./blobs/…                      the fs blob store (absent on the S3 backend)
.agit-backup/manifest.json     backend kind, schema version, timestamp, external-blobs flag
.agit-backup/hub.db            SQLite: a consistent VACUUM INTO snapshot (never a raw WAL copy)
.agit-backup/metadata.sql      Postgres: a pg_dump instead of hub.db
```

On SQLite the metadata is a consistent `hub.db` snapshot taken with SQLite's online
`VACUUM INTO`, never a raw copy of the live WAL file. On Postgres it is a `pg_dump`
(`metadata.sql`) instead. Transient files (the live `hub.db-wal`/`hub.db-shm` sidecars,
already folded into the snapshot, and any `*.lock`) are excluded.

The snapshot holds password hashes and token digests. The whole tarball is written
`0600` and is sensitive even though those digests are not reversible plaintext. Keep it
off-host.

:::caution S3/Garage blobs are external
On the S3/Garage blob backend the blobs are not on the data root, so they are **not** in
the tarball. `backup` warns loudly and records `external_blobs: true` in the manifest.
Back up Garage's own storage separately (its `garage-meta` and `garage-data` volumes
under compose, or your S3 provider's own snapshotting). On the filesystem blob backend,
`<root>/blobs` is inside the tarball and needs no separate step.
:::

## Restoring

Run `restore` with the hub stopped, and with the same `AGIT_HUB_DB` set that the target
hub uses:

```sh
sudo systemctl stop agit-hub
sudo -u agithub agit-hub restore /secure/agit-hub-2026-07-16.tgz --root /var/lib/agit-hub
sudo systemctl start agit-hub
```

Two guards refuse a foot-gun:

- **Non-empty root.** `restore` refuses to write into a data root that already holds
  data, so it never clobbers a live hub silently. Pass `--force` to overwrite it
  deliberately.
- **Cross-backend.** The manifest records the metadata backend the snapshot came from,
  and `restore` refuses to cross it: a SQLite dump into a Postgres target, or the
  reverse, is rejected. For a Postgres backup it restores into the database
  `AGIT_HUB_DB` points at, so that variable must be set to the target.

Every archive member is checked against path traversal before extraction. On SQLite the
restored row counts are verified against the counts the manifest recorded at backup time,
so a short snapshot is caught loudly rather than restoring a near-empty database.

## Invocation gotchas

- **`--root` must match the running hub.** On systemd the service root is
  `/var/lib/agit-hub`; under docker it is the data volume. Point `backup`/`restore` at
  the same root, or you snapshot or overwrite an empty directory.
- **`pg_dump` and `psql` must be on `PATH`** for the Postgres path of both commands.
- **Set the backend variables for the admin command too.** `backup` and `restore` pick
  the backend from `AGIT_HUB_DB` and `AGIT_HUB_S3_ENDPOINT` exactly as `serve` does. Run
  them in the same environment (or under the same systemd/compose service) that carries
  those variables. See [configuration](./configuration.md).
- **Restore with the hub stopped.** A live hub holds the schema write locks; restoring
  under it races the running process.

For the containerized deploy you can also snapshot the data-root volume directly with a
throwaway container, and the Postgres and Garage volumes alongside it, but the built-in
commands are the supported path.
