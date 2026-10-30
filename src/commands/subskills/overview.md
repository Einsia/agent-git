---
name: agit-overview
description: What bare agit shows - whether this folder is saved, its recent sessions, and the next step.
---

# agit (no command)

## Purpose

Run `agit` with no command to see, from local state only: the signed-in account, whether the
current folder is on and where it uploads, its recent sessions with their upload state (`not
saved`, `not uploaded yet`, `uploading`, `uploaded`), the last automatic upload's error, and
the next step. In a folder that is on it also hands sessions the Hub still lacks to the
background uploader. `agit --tui` opens the session picker instead.

## Examples

```bash
agit
agit --json
```
