---
name: agit-fetch
description: Fetch Agent repo objects and remote refs without creating a local session branch.
---

# agit fetch

## Purpose

Fetch remote history for inspection or preparation. An explicitly named peer repo that is not local may be created, have a remote configured, and be fetched. `fetch` never checks out, binds the cwd, or creates a local branch.

## Synopsis

```bash
agit fetch [repo] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[repo]` | `<owner/name>`; context/remote resolution when omitted |
| `--all` | Process every local Agent repo with an `origin` |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit fetch alice/notes
agit fetch --all
```

To turn fetched history into a writable session, run `agit fork alice/notes@main -b local-review`. A successful fetch does not prove that a branch was created.
