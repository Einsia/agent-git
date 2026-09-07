---
name: agit-doctor
description: Diagnose AgentGit local storage, runtime integrations, and optional backend connectivity.
---

# agit doctor

## Synopsis

```bash
agit doctor [--repo <owner/repo>] [--check-backend]
```

## Options

| Option | Meaning |
|---|---|
| `--repo <owner/repo>` | Inspect only this local repository and its fully matching adopted-session claims |
| `--check-backend` | Also check Hub/backend reachability and configuration |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit doctor
agit doctor --repo alice/research
agit doctor --check-backend
```

The report diagnoses problems; it does not automatically create branches or rewrite history. Follow its suggested `setup`, `login`, or explicit `new`/`import` actions.

`--repo` requires an explicit complete owner and repository name, without a branch or selector. The repository must already exist locally; a missing repository is a reference error. The option does not infer an owner from authentication, workspace context, or an incomplete claim. Repository integrity checks include its registered worktrees. Other repositories are excluded from the integrity report, and their native transcripts are not opened. Runtime, skill, authentication, Git, and keystore diagnostics remain global.

A scoped inspection does not run startup migration or the automatic upgrade network check. It refuses pending recovery for the selected repository, including checkout aliases and legacy recovery evidence, without consuming that evidence or repairing storage. Global recovery records are inspected for repository identity; records belonging to other repositories are then skipped. Recovery evidence whose repository cannot be identified also blocks inspection. `--check-backend` remains an explicit request for a network connectivity check.

Live transcript comparison uses each active claim's recorded owner, repository, and local branch. A repository with the same name under another owner, a matching tag, and uncommitted worktree changes do not replace that evidence. Missing identity, missing files, unreadable storage, and incomplete native records are reported as unavailable.

Native transcripts and committed LOG content are compared using the same snapshot of the repository's existing secret mappings. Missing mappings for protected content make the comparison unavailable. A transcript created by `resume` is compared with its own recorded byte baseline and digest, because another runtime can remint the restored history. The report distinguishes unchanged content, appended content, truncation, and rewriting. Missing or changed branch-tip evidence is reported separately from baseline integrity. Superseded claims are excluded.

These comparisons do not learn secret mappings, write dictionary files, or alter claims. Before repairing a truncated or rewritten transcript, inspect the explicit recorded branch and the native transcript, then choose an explicit import or fork.

The `secret keystore` row probes the store `secrets.keystore` selects the way a commit uses it (the OS credential store gets a throwaway entry written, read back and deleted) and unlocks the vault if one exists. A warning there means `agit secrets add` and any commit that finds a secret fail; on a machine with no desktop session, `agit config secrets.keystore file` keeps the key in a private file instead.
