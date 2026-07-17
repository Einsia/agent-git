# Deploying agit-hub

`agit-hub` is AgentGitHub: one self-contained HTTP service that hosts your team's
Agent Stores (bare git repos of AI-session transcripts), readable by people
through an embedded React SPA and pullable by agents over git smart-http. It
carries auth (cookie sessions for people, scoped tokens for git/scripts), a
per-agent ACL, an audit log, and a server-side secret scan on every push.

This guide covers the two supported ways to run it — a container behind a reverse
proxy, and a systemd service behind a reverse proxy — plus the operational parts:
TLS (why it is mandatory), the trusted-proxy story, backups, and upgrades.

Everything below uses the **real** CLI. The full surface is:

```
agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]
               [--tls] [--insecure] [--trusted-proxy IP,IP]      start the Hub
agit-hub user add <name> [--admin]                   add a user (asks for the password)
agit-hub user list                                   list users
agit-hub add <name> [--owner <user>] [--public]      new Agent Store (private by default)
agit-hub list                                        list hosted agents
agit-hub token add <name> [--user <owner>] [--agent <name>]
                   [--read|--write] [--ttl-days N]   issue an access token
agit-hub token list                                  list tokens (metadata only)
agit-hub token rm <id>                               revoke a token
```

`agit-hub --help` prints exactly this.

---

## The security model you are deploying into

Four defaults are load-bearing. The deployment has to respect them, not fight
them:

1. **Loopback by default.** With no `--host`, the hub binds `127.0.0.1:8177`
   only. It holds your team's entire transcript history; "installing it exposes
   it to the office network" is not allowed to be the default.

2. **It refuses to be unsafe.** Binding a non-loopback address in plaintext is
   refused outright (exit code 2). To bind beyond loopback you must pass **either
   `--tls` or `--insecure`**:

   ```
   $ agit-hub serve --host 0.0.0.0
   refusing to listen on 0.0.0.0 in plaintext.
   Other people on this address's network can reach it — and without TLS, login
   passwords and tokens cross the wire in plaintext ...
   ```

   `--tls` does **not** make the hub speak TLS — the hub never terminates TLS
   itself. It is a promise that *TLS is terminated in front of it* (by a reverse
   proxy). It relaxes the bind guard and marks the session cookie `Secure`.
   `--insecure` is the "plaintext on purpose, I know the price" escape hatch for a
   trusted LAN or a throwaway demo.

3. **Secrets on disk are locked down.** The data root is created `0700`;
   `users.json`, `agents.json` and `auth.json` are written `0600` (atomically,
   via a temp file + rename). Passwords are stored as argon2id hashes — the
   plaintext never touches disk. Tokens are stored as sha256 digests only; the
   token string is shown once, at issue.

4. **The real client IP comes from `--trusted-proxy`.** Behind a proxy the hub
   sees the proxy's IP as the peer. It only reads `X-Forwarded-For` from peers you
   name in `--trusted-proxy IP,IP`. Name your proxy there or the per-IP rate limit
   keys on the proxy's address, so every client shares one bucket.

### Why TLS is mandatory the moment you leave loopback

Logins send a password; git and scripts send a token; the server sends back full
session transcripts. Without TLS all of it crosses the wire in cleartext, and any
hop on the path can copy a token and then read or push your team's entire
history. That is why the hub refuses a public plaintext bind unless you force it.
**Always terminate HTTPS in front of the hub.** The two topologies below both do.

---

## Option A — Docker behind Caddy (recommended)

Files: [`Dockerfile`](../Dockerfile), [`.dockerignore`](../.dockerignore),
[`deploy/docker-compose.yml`](../deploy/docker-compose.yml),
[`deploy/Caddyfile`](../deploy/Caddyfile).

```
client ──HTTPS──▶ caddy (:443) ──HTTP──▶ hub (:8177)
```

Caddy terminates HTTPS and reverse-proxies to the hub over the internal Docker
network, forwarding `X-Forwarded-For`. Inside the container the hub binds
`0.0.0.0:8177` with `--tls` (TLS terminated in front) and trusts Caddy's fixed
address. Nothing but Caddy can reach the hub — it publishes no host ports.

### The image

The `Dockerfile` is multi-stage:

- **build** on `rust:1-slim-bookworm` runs `cargo build --release --bin
  agit-hub`. No Node is needed: the frontend (`hub-ui/dist`) is committed and
  embedded into the binary at compile time.
- **runtime** on `debian:bookworm-slim` installs `git` (the hub shells out to it
  for receive-pack / rev-list / cat-file) and `ca-certificates`, adds a non-root
  user `agithub` (uid 10001), and runs as it. The data root is a `VOLUME` at
  `/data`; because `HOME=/data`, the hub's default `--root` resolves to
  `/data/.agit-hub` for `serve` and for every admin command alike.

The default `CMD` is `serve --host 0.0.0.0 --port 8177 --tls` — the container
model from the bind guard's own guidance. `docker-compose.yml` overrides it only
to add `--trusted-proxy`.

### Bring it up

From the repo root:

```sh
HUB_DOMAIN=hub.example.com docker compose -f deploy/docker-compose.yml up -d --build
```

Point `HUB_DOMAIN`'s DNS at this host first; Caddy will fetch and renew a Let's
Encrypt certificate automatically. For a local trial, drop `HUB_DOMAIN` (it
defaults to `localhost`, which Caddy serves with its own trusted-locally cert);
for any other private name or bare IP, uncomment `tls internal` in the
`Caddyfile`.

### First admin, first agent, first token

The password prompt needs a TTY, so run `exec` **without** `-T`:

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

No `--root` is needed on these `exec` commands: `$HOME=/data` in the image makes
every subcommand resolve the same data root.

Verify and publish from a client:

```sh
docker compose -f deploy/docker-compose.yml exec hub agit-hub user list
docker compose -f deploy/docker-compose.yml exec hub agit-hub list

# from a machine with the agit client and the token from step 3:
agit -a remote add origin https://hub.example.com/payments.git
agit -a push -u origin main
#   git prompts for a username/password — put the TOKEN in the password field
#   (the username can be anything).
```

Read-only credentials for pulling agents are the same command without `--write`
(read is the default): `token add reader --user you --agent payments --read
--ttl-days 90`.

---

## Option B — systemd behind a local reverse proxy

File: [`deploy/agit-hub.service`](../deploy/agit-hub.service).

For a non-container host. The hub binds **loopback** `127.0.0.1:8177` (which needs
no `--tls`/`--insecure`) and a reverse proxy on the *same host* terminates HTTPS
and forwards to it. `--trusted-proxy 127.0.0.1` lets the hub read the proxy's
`X-Forwarded-For`.

### Install

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

The unit runs `agit-hub serve --host 127.0.0.1 --port 8177 --root
/var/lib/agit-hub --trusted-proxy 127.0.0.1`, restarts on failure, and is
sandboxed: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
`PrivateTmp`, `PrivateDevices`, the `ProtectKernel*`/`ProtectControlGroups`
family, an empty `CapabilityBoundingSet`, `SystemCallFilter=@system-service`,
`RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`, and `MemoryDenyWriteExecute`.
Verify it before deploying with `systemd-analyze verify
/etc/systemd/system/agit-hub.service`.

### Admin commands — mind the `--root`

The service data root is `/var/lib/agit-hub`. Manual admin commands must point at
the **same** root and run as the service user, or they will read/create a
different, empty directory:

```sh
sudo -u agithub agit-hub user add you --admin       --root /var/lib/agit-hub
sudo -u agithub agit-hub add payments --owner you   --root /var/lib/agit-hub
sudo -u agithub agit-hub token add ci-writer --user you --agent payments \
                                     --write --ttl-days 90 --root /var/lib/agit-hub
```

### The proxy in front

Any TLS-terminating proxy works; it must forward `X-Forwarded-For`. A minimal
nginx server block:

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

`$proxy_add_x_forwarded_for` appends the real client IP; the hub, trusting
`127.0.0.1`, reads it back out. If your proxy sits on a different host, change the
unit's bind to that interface, add `--tls`, and set `--trusted-proxy` to the
proxy's address.

---

## Trusted proxy / X-Forwarded-For, precisely

The hub uses the request's source IP for its per-IP rate limit, and would forge
easily if it trusted `X-Forwarded-For` blindly. So:

- With **no** `--trusted-proxy`, it ignores `X-Forwarded-For` entirely and keys on
  the raw peer IP.
- With `--trusted-proxy` set, and only when the peer is one of those addresses, it
  walks `X-Forwarded-For` right-to-left and takes the first address that is *not*
  a trusted proxy — the real client. A malformed or fully-trusted chain falls back
  to the peer.

So the rule is simple: set `--trusted-proxy` to the address(es) of the proxy that
connects to the hub — Caddy's fixed `172.28.0.2` in Option A, `127.0.0.1` in
Option B — and nothing else.

---

## Backups

All state is in one directory (the data root: `/data/.agit-hub` inside the
container's `hub-data` volume, or `/var/lib/agit-hub` under systemd). Back up the
whole thing:

- `users.json`, `agents.json`, `auth.json` — users, per-agent ACL/metadata,
  token digests. `0600`; they contain password hashes and token digests, so treat
  the backup as a secret.
- `audit.log` — the append-only audit trail.
- `<name>.git/` — one bare repo per agent, the actual transcript history.

The hub writes those JSON files atomically (temp file + rename), so a filesystem
snapshot or a plain copy is internally consistent. For the containerised deploy,
back the named volume up by running a throwaway container against it:

```sh
docker run --rm -v deploy_hub-data:/data -v "$PWD":/backup debian:bookworm-slim \
  tar czf /backup/agit-hub-$(date +%F).tgz -C /data .
```

(`deploy_hub-data` is the compose-prefixed volume name — confirm with `docker
volume ls`.) Restore by extracting the same tarball back into the volume while the
hub is stopped. Under systemd, stop the service and `tar` up `/var/lib/agit-hub`.
Keep backups off-host; the token digests and password hashes in them are
sensitive even though they are not reversible plaintext.

---

## Upgrades

The binary is self-contained (the frontend is compiled in), and the on-disk
formats are read leniently — a hand-written or older `agents.json` reads as
active/private rather than failing. Upgrading is replace-and-restart:

**Docker:** rebuild and roll the hub; the volume carries the data across.

```sh
git pull
HUB_DOMAIN=hub.example.com docker compose -f deploy/docker-compose.yml up -d --build
```

**systemd:** drop in the new binary and restart.

```sh
sudo install -m 0755 target/release/agit-hub /usr/local/bin/agit-hub
sudo systemctl restart agit-hub.service
```

There is no migration step. On start the hub reports anything needing attention:
users with no accounts, old unowned repos to claim (`agit-hub add <name> --owner
<user>`), and legacy tokens with no owner (dead under the current ACL — reissue
with `agit-hub token add … --user <owner>` and drop the old ones with `agit-hub
token rm <id>`).

Back up the data root before any upgrade.
