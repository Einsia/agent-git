---
sidebar_position: 1
title: Deploying the hub
---

# Deploying the hub

`agit-hub` is one self-contained HTTP binary. It hosts your team's agent stores
(bare git repos of session transcripts), serves the web UI to browse them, and
answers git smart-http for the CLI to push and pull. It carries authentication,
a per-agent access check, an audit log, and a server-side secret scan on every
push. The frontend is compiled into the binary, so there is nothing else to serve.

This page stands one up: the storage backends, running it under docker compose or
systemd, and the TLS reverse proxy in front. For the environment variables it reads,
see [configuration](./configuration.md). For running it day to day, see
[operations](./operations.md).

## What runs where

The hub keeps its state in two places:

- **Metadata** (users, agents, tokens, ACLs, the audit trail) in a database. SQLite
  by default, Postgres for production, selected by `AGIT_HUB_DB`.
- **Blobs** (large content-addressed objects) in a blob store. The local filesystem
  by default, Garage or any S3-compatible store when configured, selected by
  `AGIT_HUB_S3_ENDPOINT`.

Both have zero-config defaults, so a hub with neither variable set runs on SQLite
and local-disk blobs under its data root. Move to Postgres and S3 by setting the
variables in [configuration](./configuration.md); the hub creates and migrates its
own tables at boot, so there is no init SQL.

## The safety gate you deploy into

Four defaults are load-bearing.

1. **Loopback by default.** With no `--host`, the hub binds `127.0.0.1:8177` only.
   It holds your team's entire transcript history, so exposing it to the network is
   never the default.

2. **It refuses to be unsafe.** Binding a non-loopback address in plaintext exits
   with code 2:

   ```
   $ agit-hub serve --host 0.0.0.0
   refusing to listen on 0.0.0.0 in plaintext.
   Other people on this address's network can reach it — and without TLS, login passwords and
   tokens cross the wire in plaintext ...
   ```

   To bind beyond loopback, pass **either `--tls` or `--insecure`**. `--tls` does
   not make the hub speak TLS (it never terminates TLS itself). It is a promise that
   a reverse proxy terminates TLS in front of it; it relaxes the bind guard and marks
   the session cookie `Secure`. `--insecure` is the deliberate plaintext escape hatch
   for a trusted LAN or a throwaway demo.

3. **Secrets on disk are locked down.** The data root is created `0700`. On SQLite the
   metadata database (`hub.db` and its WAL sidecars) is written `0600`; with Postgres
   that metadata lives in the database instead. Passwords are stored as argon2id hashes
   and tokens as sha256 digests, so the plaintext of neither touches disk.

4. **The real client IP comes from `--trusted-proxy`.** Behind a proxy the hub sees
   the proxy's address as the peer. It only reads `X-Forwarded-For` from peers you
   name in `--trusted-proxy IP,IP`. Name your proxy there, or the per-IP rate limit
   keys on the proxy's address and every client shares one bucket.

:::danger
Logins send a password, git and scripts send a token, and the server sends back full
session transcripts. Without TLS all of it crosses the wire in cleartext. Always
terminate HTTPS in front of the hub the moment you leave loopback.
:::

## Option A: docker compose behind a proxy

The repo ships a compose file under `deploy/` that bundles four services: the reverse
proxy (TLS in front), **hub**, **postgres** (the production metadata backend), and
**garage** (S3-compatible blob storage). Only the proxy publishes host ports; postgres
and garage stay internal to the compose network.

```
client ──HTTPS──▶ proxy (:443) ──HTTP──▶ hub (:8177)
```

Inside the container the hub binds `0.0.0.0:8177` with `--tls` (TLS terminated in
front) and trusts the proxy's fixed address via `--trusted-proxy`. The image runs as a
non-root user; its `HOME` is the data volume, so the default `--root` resolves the same
for `serve` and for every admin command.

Bring it up from the repo root, pointing DNS at the host first so the proxy can fetch a
certificate:

```sh
HUB_DOMAIN=agit.anggita.org docker compose -f deploy/docker-compose.yml up -d --build
```

Postgres is wired up by default. Set a strong `PGPASSWORD` for a real deploy. Garage
sits idle until you point the hub at it; to store blobs there, set the `AGIT_HUB_S3_*`
block and run Garage's one-time init (layout, bucket, key). Both are covered in
[configuration](./configuration.md).

Create the first admin, a private agent store, and a write token. The password prompt
needs a TTY, so run `exec` without `-T`:

```sh
# the first user must be a site admin
docker compose -f deploy/docker-compose.yml exec hub agit-hub user add you --admin

# a private agent store (private is the default; --public to publish)
docker compose -f deploy/docker-compose.yml exec hub agit-hub add payments --owner you

# a scoped, expiring write token (printed ONCE; only its sha256 digest is stored)
docker compose -f deploy/docker-compose.yml exec hub \
  agit-hub token add ci-writer --user you --agent payments --write --ttl-days 90
```

No `--root` is needed on these `exec` commands: the container's `HOME` makes every
subcommand resolve the same data root. Accounts, tokens, and agents are covered under
[accounts](../hub/accounts.md) and [tokens](../hub/tokens.md).

## Option B: systemd behind a local reverse proxy

For a non-container host. The hub binds loopback `127.0.0.1:8177` (which needs no
`--tls`/`--insecure`) and a reverse proxy on the same host terminates HTTPS and forwards
to it. `--trusted-proxy 127.0.0.1` lets the hub read the proxy's `X-Forwarded-For`.

Install the binary, a non-root service user, and the unit from `deploy/`:

```sh
# 1. the binary
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub

# 2. the service user (matches User= in the unit)
sudo useradd --system --home-dir /var/lib/agit-hub --shell /usr/sbin/nologin agithub

# 3. the unit (StateDirectory=agit-hub creates /var/lib/agit-hub 0700)
sudo install -m 0644 deploy/agit-hub.service /etc/systemd/system/agit-hub.service
sudo systemctl daemon-reload
sudo systemctl enable --now agit-hub.service
```

The unit runs `agit-hub serve --host 127.0.0.1 --port 8177 --root /var/lib/agit-hub
--trusted-proxy 127.0.0.1`, restarts on failure, and is sandboxed (`NoNewPrivileges`,
`ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, an empty `CapabilityBoundingSet`,
`SystemCallFilter=@system-service`, and more). Verify it before deploying:

```sh
systemd-analyze verify /etc/systemd/system/agit-hub.service
```

:::caution Mind the `--root` on admin commands
The service data root is `/var/lib/agit-hub`. Manual admin commands must point at the
same root and run as the service user, or they read or create a different, empty
directory:

```sh
sudo -u agithub agit-hub user add you --admin --root /var/lib/agit-hub
sudo -u agithub agit-hub add payments --owner you --root /var/lib/agit-hub
```
:::

## The reverse proxy and TLS

Any TLS-terminating proxy works. It must forward `X-Forwarded-For` and must not cap or
buffer the request body: git pushes and full transcripts are large, and smart-http is a
streaming protocol. A minimal nginx server block:

```nginx
server {
    listen 443 ssl;
    server_name agit.anggita.org;

    ssl_certificate     /etc/letsencrypt/live/agit.anggita.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/agit.anggita.org/privkey.pem;

    # git pushes and full transcripts can be large; do not cap the body.
    client_max_body_size 0;

    location / {
        proxy_pass         http://127.0.0.1:8177;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;

        # smart-http streams; don't buffer it into a file first.
        proxy_request_buffering off;
        proxy_buffering        off;
        proxy_http_version     1.1;
    }
}
```

`$proxy_add_x_forwarded_for` appends the real client IP, which the hub reads back out
because it trusts `127.0.0.1`. If the proxy sits on a different host, change the unit's
bind to that interface, add `--tls`, and set `--trusted-proxy` to the proxy's address.

For key-based auth to be replay-safe across hubs, set `AGIT_HUB_PUBLIC_URL` to this
proxy's public origin (`https://agit.anggita.org`). See
[configuration](./configuration.md) for why.

## Upgrades

The binary is self-contained and runs its own database migrations at boot (idempotent,
gated by a schema-version row), so upgrading is replace-and-restart with no manual
migration.

**Docker:** rebuild and roll the hub; the volume carries the data across.

```sh
git pull
HUB_DOMAIN=agit.anggita.org docker compose -f deploy/docker-compose.yml up -d --build
```

**systemd:** keep the previous binary as your rollback, drop in the new one, and
restart.

```sh
sudo cp -p /usr/local/bin/agit-hub /usr/local/bin/agit-hub.bak   # rollback point
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub
sudo systemctl restart agit-hub.service
```

A migration runs inside a transaction and fails closed: if one cannot complete, the boot
aborts and nothing is half-written, so the previous binary starts cleanly against the
unchanged data. `systemctl is-active agit-hub` after a restart confirms it came up.
Back up the data root before any upgrade; see [backup and restore](./backup-restore.md).
