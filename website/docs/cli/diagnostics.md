---
sidebar_position: 10
title: Diagnostics
---

# Diagnostics

Two commands report on the install. `agit doctor` is a fast health check you paste into a bug report;
`agit debug` writes a full, redacted diagnostic bundle you attach. Both collect the same state, and both
redact secrets before anything is written or printed. Nothing is uploaded.

## Health check

```bash
agit doctor
```

`agit doctor` prints a human-readable health check and environment summary: the runtimes on `PATH`, your
git identity, where the active store stands against its remote, the watch daemon's state, and the like.
It takes no arguments. Run it first when something is off, and paste its output when you report a problem.

## Diagnostic bundle

```bash
agit debug
```

`agit debug` writes the full bundle: sectioned text plus a machine-readable `debug.json`, with the doctor
summary on top. It echoes the bundle so you review exactly what it collected before you share it.

| Flag | Effect |
|---|---|
| `--out <dir>` | Write the bundle to a named directory. |
| `--rerun "<subcmd>"` | Re-run a failing agit subcommand under trace and capture it in the bundle. |

```bash
agit debug --out ./agit-debug
agit debug --rerun "a push"
```

## What is collected, and that it is redacted

The bundle collects the runtimes agit sees and their session dumps, your git config and remotes, the
store's state and its remotes, every `AGIT_*` environment variable, and the resolved active agent. It does
not collect session transcript contents.

Everything passes through redaction before it is written. Two layers run:

- Field-specific redaction at the source: remote URLs have their credentials stripped, and every `AGIT_*`
  variable's value is masked unconditionally (the name is kept, the value shown as `[redacted]`), because
  a value can carry a token.
- A final pass over the entire assembled bundle: every URL with a scheme has its credentials stripped,
  and any line the secret scanner flags is replaced with a redaction marker.

Because the bundle is redacted and echoed, you can read it before attaching it. See
[Reporting problems](../hub/reporting-problems.md) for where to send it.
