---
sidebar_position: 11
title: Sessions run in another directory
---

# Sessions run in another directory

agit captures a session for the repository it worked on. You do not have to launch the agent from the repo
root: agit attributes a session by where it ran and which files it touched.

## Captured automatically

`agit a snap` and `agit watch` capture these without any extra step:

- **Run inside the repo.** A session whose working directory is the repo root or a subdirectory of it
  (`cd frontend && claude`) is captured, the same way `git` finds its repository by walking up to `.git`.
  This holds whether the session edited files or only read them.
- **Edited files here.** A session launched above or outside the repo (a parent or workspace directory)
  that edited a file inside this repo is captured, because it demonstrably worked here. agit reports what
  it pulled in and from where.

A session that edited files in two repositories belongs to each, and is captured into each when you snap
there. Every auto-captured session goes through the same secret scan as a normal capture.

## What agit asks about

One case is genuinely ambiguous: a session launched **above** the repo that only **read** files here and
edited nothing. It cannot be told apart from a review that happened to glance at this repo on its way
through a workspace, so agit never claims it silently.

- In an interactive terminal, `snap` and `watch` ask once per directory: `N session(s) ran in ~/dev and
  read files here but edited none. Capture for this repo? [y/N]`.
- Without a terminal (a script, or the `watch` daemon), agit prints a note pointing at `agit relocate` and
  captures nothing ambiguous.

## Relocate

`agit relocate` is the manual control. It lists every session that ran in another directory but plausibly
belongs here (every case above, read-only included) and moves them in on confirmation:

```bash
agit relocate
```

```
Sessions that ran elsewhere but belong in /home/you/code/web:
  claude-code · /home/you/code · 2 hours ago
      "add a rate limiter"
bring these 1 session(s) into /home/you/code/web? [Y/n]
```

Reach for it for the read-only-above case, a batch move, or a session launched in an unrelated directory
that touched this repo (the one case auto-capture does not scan for).

| Flag | Effect |
|---|---|
| `<session>` | Relocate one session, matched by id, transcript path, or a substring of its recorded directory. |
| `--to <path>` | Override the destination. It must be a git work tree; the session installs under a slug derived from it. |
| `--yes` (`-y`) | Skip the confirmation. |

The destination defaults to this repo's root. Relocate moves history into the store, so it never proceeds
unattended: a non-interactive shell without `--yes` is treated as "no".

## Related

`agit resume --relocate` handles the adjacent case: continuing one recorded session against the current
checkout when it is the same project moved, rewriting the session's recorded paths onto it. See
[Resuming sessions](./resuming.md).
