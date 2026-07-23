---
sidebar_position: 4
title: Operations
---

# Operations

Running the hub day to day: the diagnostic command, the version and metrics endpoints,
the request-correlation id that ties a client error to a server log line, and how to
read the two denials you will actually see (a refused push and a failed login).

## `agit-hub doctor`

The operator diagnostic. It reports what a `serve` would boot with, reading the same
`AGIT_HUB_DB` and `AGIT_HUB_S3_*` from the environment, and probes that the backends are
reachable:

```sh
sudo -u agithub agit-hub doctor --root /var/lib/agit-hub
```

The report has five sections:

- **VERSION**: hub version, build sha (when compiled in), and schema version.
- **DATABASE**: backend (`sqlite`/`postgres`), the target (host and db only, never
  credentials), per-table row counts, and a health line. The row counts double as the
  reachability probe: a table that answers is a table the hub reached.
- **BLOB STORAGE**: backend, and for S3 the endpoint, bucket, and region, with the
  access and secret keys shown as `set (masked)` and never read into the report. A
  reachability probe HEADs a well-formed absent object.
- **DATA ROOT**: path, size on disk, and free space.
- **CONFIG**: registration mode and the listen shape.

Credentials are redacted twice: explicit field masks drop the DB userinfo and hide the S3
keys, and a final pass runs the whole report through the secret scanner and masks any
line it flags. A non-zero exit means a backend could not be opened, which is itself the
most important diagnostic. `agit-hub doctor` is the server-side counterpart to the
client's [diagnostics](../cli/diagnostics.md).

## `GET /api/version`

Public, no auth. Returns the version, the build sha (or `null` when it was not set at
compile time, never a fabricated value), and the schema version:

```sh
curl -s https://hub.example.com/api/version
```

```json
{"version":"0.2.1","build_sha":"…","schema_version":37}
```

Use it as an uptime and version check from outside, and to confirm what version a client
actually talked to.

## `GET /metrics`

Prometheus text exposition, **admin-gated**. It is served through the same auth as every
other route, and a non-admin (or anonymous) caller gets the same `404` a missing route
would, so `/metrics` is not even discoverable without an admin credential. Scrape it with
an admin token in the password field:

```sh
curl -s -u x:$ADMIN_TOKEN https://hub.example.com/metrics
```

The series it exposes:

| Metric | Type | Meaning |
| --- | --- | --- |
| `agit_hub_build_info{version}` | gauge | Always 1; carries the version label. |
| `agit_hub_uptime_seconds` | gauge | Seconds since this process started serving. |
| `http_requests_total{method,status}` | counter | Requests by method and status class. |
| `http_request_duration_seconds` | histogram | Request latency. |
| `auth_attempts_total{result}` | counter | Auth attempts by outcome. |
| `git_push_total{result}` | counter | Push attempts, `accepted` vs `rejected`. |
| `secret_scan_rejects_total` | counter | Pushes refused in-process by the secret scan. |

Issue the scraping credential like any other admin token
(`agit-hub token add --user <admin>`); see [tokens](../hub/tokens.md).

## X-Request-Id correlation

Every response carries an `X-Request-Id` header, a 16-hex id minted server-side once per
request. A caller-supplied `X-Request-Id` is deliberately not honored, so a client cannot
forge or collide ids. For a JSON error body (status >= 400) the same id is folded into
the object as a `request_id` field, so the client and web UI can surface it.

That id also tags the structured log line for the request (`request_id=…`). When a user
reports an error, ask for the `request_id` from the error and grep the logs for it: it
ties the client-visible failure to the exact server-side line, including the auth and
push decisions below. This is what [reporting problems](../hub/reporting-problems.md)
asks users to include.

## Reading a denied push

An unauthorized push (receive-pack) is refused at the authorization gate before its pack
is ever read into memory. Three things record it:

- an **audit-log** deny entry with the actor, the scoped agent, and the action;
- a **structured log** line, `git push rejected`, carrying `agent`, `actor`, and the
  `reason`;
- the **metric** `git_push_total{result="rejected"}`.

The client sees a `401` with a `WWW-Authenticate` header, because a git client only
prompts for credentials on a 401. So a push that keeps re-prompting for a password is
almost always an ACL denial, not a wrong password: check the token's scope and the
agent's visibility. An authorized push increments `git_push_total{result="accepted"}` and
logs `git push accepted`; the server-side secret scan then runs in the out-of-process
pre-receive hook, and a scan rejection is authoritative in the audit log.

## Reading a failed auth

A failed login logs a `login failed` warning (with the normalized username, never the
password), writes a `LOGIN_FAILED` audit entry, increments
`auth_attempts_total{result="login_fail"}`, and returns a generic `401 wrong username or
password`. The message is intentionally the same whether the user does not exist or the
password is wrong, so it hands a brute-forcer no username dictionary.

A token that is presented but resolves to nobody (expired, revoked, or unknown) logs
`token denied`, without the header contents, and increments the denied auth counter. A
token over its per-token request budget gets a `429` with `Retry-After`; that budget is
per token, not per address, so one noisy token cannot exhaust everyone's quota. Argon2 is
deliberately slow, so logins are concurrency-gated; a burst of logins queues rather than
pegging every core.

If auth is failing hub-wide, start with `agit-hub doctor` (is the database reachable?),
then `GET /api/version` (is the process serving?), then the request's `X-Request-Id` in
the logs.
