---
sidebar_position: 2
title: Configuration
---

# Configuration

The hub is configured by command-line flags on `agit-hub serve` and by environment
variables. Flags set the listen shape and the safety posture; environment variables
select the storage backends and a few behavioral toggles. The hub reads no config
file. Whatever you set for `serve`, set the same for `agit-hub doctor`, `backup`, and
`restore`, so an admin command talks to the same backends the server does.

## Serve flags

```
agit-hub serve [--host 127.0.0.1] [--port 8177] [--root ~/.agit-hub]
               [--tls] [--insecure] [--trusted-proxy IP,IP]
               [--open-registration] [--public-url URL]
```

| Flag | Default | Effect |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Interface to bind. Loopback holds the team's history off the network by default. |
| `--port` | `8177` | Listen port. |
| `--root` | `$HOME/.agit-hub` | Data root: bare repos, `audit.log`, SQLite `hub.db`, fs blobs. |
| `--tls` | off | Promise that a proxy terminates TLS in front. Relaxes the plaintext bind guard and marks the cookie `Secure`. Does not make the hub speak TLS. |
| `--insecure` | off | Deliberately listen in plaintext beyond loopback. |
| `--trusted-proxy` | none | Comma-separated proxy IPs whose `X-Forwarded-For` the hub trusts for the client address. |
| `--open-registration` | off | Enable self-service signup (`POST /api/register`). |
| `--public-url` | none | This hub's canonical base URL; pins the key-auth audience. Same as `AGIT_HUB_PUBLIC_URL`. |

`--tls`/`--insecure` and `--trusted-proxy` are covered in [deploying](./deploying.md).

## Metadata backend: `AGIT_HUB_DB`

Selects where user, agent, token, and ACL metadata lives.

- **Unset, or any non-URL value:** the SQLite `hub.db` under the data root, written
  `0600`. Zero-config; good for a single host or a trial.
- **A `postgres://` or `postgresql://` URL:** Postgres, the production backend.

```sh
AGIT_HUB_DB=postgres://agithub:STRONGPASS@postgres:5432/agithub
```

The hub creates and migrates its own tables at boot, so there is no init SQL. A bad URL
or an unreachable Postgres is a clear error at boot, not on the first request.

## Blob backend: the `AGIT_HUB_S3_*` variables

Selects where large content-addressed objects live. Left unset, blobs are stored on the
filesystem under `<root>/blobs`. To store them in Garage or any S3-compatible store, set
`AGIT_HUB_S3_ENDPOINT` (non-empty), which turns on the S3 backend and makes the rest
required:

| Variable | Required with S3 | Default |
| --- | --- | --- |
| `AGIT_HUB_S3_ENDPOINT` | selects S3 | fs backend when unset |
| `AGIT_HUB_S3_BUCKET` | yes | none |
| `AGIT_HUB_S3_ACCESS_KEY` | yes | none |
| `AGIT_HUB_S3_SECRET_KEY` | yes | none |
| `AGIT_HUB_S3_REGION` | no | `garage` |

Path-style addressing is always on (Garage requires it).

:::caution Fail-closed at boot
If `AGIT_HUB_S3_ENDPOINT` is set but the bucket or a key is missing or empty, the hub
errors at boot. It never silently falls back to local disk, so a misconfigured endpoint
can never quietly write blobs to the wrong place.
:::

Garage does not auto-create its layout, bucket, or key. A Garage-backed deploy needs a
one-time init after the first `up`: assign a storage layout to the node, create the
bucket, mint an access key, and grant it read+write. Run those with
`docker compose ... exec garage /garage ...`, then bring the hub up with the key
material in `AGIT_HUB_S3_ACCESS_KEY` / `AGIT_HUB_S3_SECRET_KEY`.

## `AGIT_HUB_PUBLIC_URL`: pin the key-auth audience

Set this to the hub's own canonical base URL (`scheme://host[:port]`, no path), for
example `https://hub.example.com`. The `--public-url` flag is equivalent; a trailing
slash is trimmed.

Key-based auth signs a challenge for a specific hub. The handler at `POST /api/auth/key`
checks the signed assertion against this audience. A value configured by the operator is
server-controlled and cannot be spoofed by a request header, so a signature captured for
this hub can never be replayed against a different hub. When `AGIT_HUB_PUBLIC_URL` is
unset, the handler falls back to the request `Host` header, which is best-effort only.

:::danger Set this on any deployment reachable at more than one name
Behind a reverse proxy the hub cannot know its own public origin. Without
`AGIT_HUB_PUBLIC_URL`, an attacker who can steer a client's request `Host` (or who runs
a second hub) has room to replay a key-auth assertion across hubs. Pin it to the exact
public origin your proxy serves. See
[authentication](../integration/authentication.md) for the client side of key auth.
:::

## `AGIT_HUB_REGISTRATION`: self-service signup

Accounts are created by a site admin (`agit-hub user add`) by default; the hub is
invite-only. To let people create their own accounts, set `AGIT_HUB_REGISTRATION` to
`1`, `true`, `open`, or `yes` (or pass `--open-registration`). This opens
`POST /api/register`, which creates a normal, non-admin account and logs it in.
Registration can never grant admin; that stays CLI-only. The startup banner reports the
current mode (`signup: open` or `invite-only`).

## Other environment variables

| Variable | Purpose | Default |
| --- | --- | --- |
| `AGIT_HUB_BASE_URL` | Base URL prefixed onto the email-verification and password-reset links the hub emits. Unset emits a bare path for the operator to prefix. | empty (bare path) |
| `AGIT_HUB_PROVENANCE_ENFORCE` | `1`/`true`/`yes`/`on` rejects pushes whose provenance does not verify. Absent/blank/false records the finding only. | record-only |
| `AGIT_HUB_LOG` | `tracing` filter directive, e.g. `agit_hub=debug,info`. | `info` |
| `AGIT_HUB_LOG_FORMAT` | `pretty` (human) or `json` (one object per line, for a log pipeline). | `pretty` |

`AGIT_HUB_BASE_URL` sets the link base for [accounts](../hub/accounts.md) email flows;
`AGIT_HUB_PROVENANCE_ENFORCE` gates the push-time check described under
[provenance](../integration/provenance.md). The two log variables are read once at boot;
logs go to stderr so the startup banner on stdout stays clean.
