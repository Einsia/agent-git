# Debug info collection (CLI users + hub users)

Date: 2026-07-22
Status: approved, to build

## Goal

An easy, comprehensive, and safe way for a user who hits a problem to hand us the context to
debug it. Two audiences:

- **CLI users** (`agit`): a local diagnostic bundle they generate and attach to a bug report.
- **Hub users** (people using a hub via the web UI or a client): they cannot see the server, so
  the design is correlation-first, plus a browser-side report.

"Safe" is non-negotiable: nothing collected may leak a secret. Everything runs through explicit
field redaction AND agit's existing secret scanner (`src/scan.rs`) before it is shown or written.
Nothing leaves the machine unless the user chooses to send it.

## Commands

- **`agit doctor`** — a fast health check + environment summary, human-readable, printed to the
  terminal. Runs checks (codex/claude on PATH, git identity set, store vs remote, daemon state)
  and prints a short pass/warn list. This is the thing a user pastes into an issue.
- **`agit debug [--out <file>] [--rerun "<cmd>"]`** — the full bundle. Writes a sectioned set of
  text files plus a machine-readable `debug.json`, with the `doctor` summary on top, to `--out`
  (default a timestamped file in the cwd). `--rerun` re-executes a failing command under
  `RUST_LOG=debug RUST_BACKTRACE=full` and captures stdout+stderr+backtrace (usually the single
  most useful artifact).
- **`agit-hub doctor`** — the operator/self-host diagnostic (version, backend, schema, row counts,
  storage, config shape, journal tail, health checks). Lower priority: only matters where teams
  self-host; for a hosted hub it is a maintainer-side tool.

## What the CLI bundle collects (as much as possible, redacted)

- **Build & platform**: agit version, build git sha + date, rustc/edition, how installed; OS
  distro + kernel + arch + libc; shell + version; `$TERM`, locale; node + npm version.
- **Runtimes**: claude-code + codex installed? version + resolved path; `agit adapter`; default here.
- **git**: git version; `user.email`/`user.name` (the snap identity); is-a-repo? toplevel, branch,
  HEAD, `status` counts, remotes (credentials stripped); relevant `core.*`/`credential.*`.
- **agit env**: resolved `$AGIT_HOME`, `$HOME`; every `AGIT_*` var (names always, values redacted);
  `.agit.toml` (URLs credential-stripped); `.agit/` active pointer; `agit shadow status`; store hooks.
- **agent & store**: `agit a list`, `agit a status`; active agent aid/name/path, store
  `git log --oneline -10`, store `git status`, remotes (stripped), branch, ahead/behind, committer
  email; session count per env; store size; the `agit a log` divergence-tree summary.
- **runtime dumps**: `~/.claude/projects/*` and `~/.codex/sessions/*` layout + session COUNTS
  (never contents); whether this repo's slug dir exists (the wrong-cwd class of bug); newest mtime.
- **daemon**: `agit watch --status`, pid, interval, watch-log tail.
- **connectivity**: configured remotes; hub reachable (HEAD + TLS); token present? (redacted); hub URL;
  the `X-Request-Id` of any failed hub call (for correlation, see below).
- **the failure**: `--rerun` output under trace.

## Hub users: correlation-first

- **Request IDs (server).** The hub attaches an `X-Request-Id` (correlation id) to every response and
  logs it server-side with the method/route/status. On an error, the web UI and the client surface
  the id. A user reports the id; we grep the server logs for it. No server access needed by the user.
  This also provides real per-request tracing, filling part of the observability gap.
- **Web UI "Report a problem".** A menu item / error-page button that collects, for the user to copy
  or download and send: browser + viewport + current route; hub version (from `GET /api/version`) +
  the UI build sha; session state (logged in as who, no token, or anonymous); a ring buffer of the
  recent FAILED API requests (method, path, status, `X-Request-Id`, redacted response body); captured
  client-side console errors.
- **Client of the hub** is the CLI bundle above; its connectivity + `--rerun` sections capture a
  failed push/pull and record the `X-Request-Id` the hub returned.

## Redaction (layered, mandatory)

1. Explicit field masks: passwords, tokens, ed25519/x25519 keys, DB/remote URLs (keep host, drop
   creds), S3 keys, cookie/session values.
2. A final pass of the whole assembled bundle through `scan::scan_text` (agit's secret scanner), so a
   secret that slipped into a log line or env value is caught and masked.
3. The bundle prints/echoes exactly what it collected, so the user reviews before sending.

## Format

A directory or `.tgz` of sectioned text files (`platform.txt`, `git.txt`, `agit.txt`, `store.txt`,
`runtimes.txt`, `connectivity.txt`, `rerun.txt`) plus `debug.json` (the same data, machine-readable),
with the `doctor` summary as `SUMMARY.txt` on top. The hub web report is a single copyable JSON blob
plus a "Download report" button.

## Build order

- **Wave A (CLI)**: `agit doctor` + `agit debug` (bundle + `--rerun`), redaction via the scanner,
  record hub `X-Request-Id`. Client-only; independent.
- **Wave B (hub server)**: `X-Request-Id` middleware (attach + log), `GET /api/version`,
  `agit-hub doctor` (operator). Server-only; independent of A (A degrades gracefully if B is absent).
- **Wave C (web UI)**: the "Report a problem" bundle (failed-request ring buffer + report view).
  Depends on B (request ids + `/api/version`).

## Non-goals

- No telemetry / auto-upload by default. The bundle is user-generated and user-sent; an opt-in
  `--upload` to the hub is a possible later add, after the file flow is trusted.
- `agit-hub doctor` is not the focus; it is the small operator piece of Wave B.
