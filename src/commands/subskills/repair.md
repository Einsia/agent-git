---
name: agit-repair
description: Put back values an older agit replaced with placeholders in a repository's files; real credentials stay replaced.
---

# agit repair

## Purpose

Older agit versions replaced some values that are not credentials (checksums, data URIs, links)
with `{{AGIT_SECRET_V…}}` placeholders in the files they published, which broke those files for
everyone who downloaded them. `agit repair placeholders` puts the values back from this device's
dictionary and protects each file again under the current check. It lands as one local commit on
the file line; history isn't rewritten and nothing is published until a push.

## Synopsis

```bash
agit repair placeholders OWNER/REPO[@BRANCH] [--dry-run]
```

## Examples

```bash
agit repair placeholders alice/project@main --dry-run
agit repair placeholders alice/project
```

## Before pushing

The repaired files hold everything the current check doesn't treat as a credential. Show the user
the listed files and let them decide; run `agit push OWNER/REPO` only after they say so.

A placeholder this device has no value for is listed by file and line, and the command exits with
a precondition error: sign in here so the dictionary syncs, or run the repair on the device that
published the files.
