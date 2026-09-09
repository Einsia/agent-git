---
name: agit-doctor
description: Diagnose AgentGit local storage, runtime integrations, and optional backend connectivity.
---

# agit doctor

## Synopsis

```bash
agit doctor [--repo <owner/repo>] [--check-backend] [--deep]
```

## Options

| Option | Meaning |
|---|---|
| `--repo <owner/repo>` | Inspect only this local repository and its fully matching adopted-session claims |
| `--check-backend` | Also check Hub/backend reachability and configuration |
| `--deep` | Inspect committed session integrity throughout locally reachable history |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

## Examples

```bash
agit doctor
agit doctor --repo alice/research
agit doctor --check-backend
agit doctor --repo alice/research --deep
```

The report diagnoses problems; it does not automatically create branches or rewrite history. Follow its suggested `setup`, `login`, or explicit `new`/`import` actions.

`--repo` requires an explicit complete owner and repository name, without a branch or selector. The repository must already exist locally; a missing repository is a reference error. The option does not infer an owner from authentication, workspace context, or an incomplete claim. Repository integrity checks include its registered worktrees. Other repositories are excluded from the integrity report, and their native transcripts are not opened. Runtime, skill, authentication, Git, and keystore diagnostics remain global.

Doctor does not run startup migration or the automatic upgrade network check. It refuses pending recovery without consuming that evidence or repairing storage. With `--repo`, this includes the selected repository's checkout aliases and legacy recovery evidence. Global recovery records are inspected for repository identity; records belonging to other repositories are then skipped. Recovery evidence whose repository cannot be identified also blocks scoped inspection. `--check-backend` remains an explicit request for a network connectivity check.

Live transcript comparison uses each active claim's recorded owner, repository, and local branch. A repository with the same name under another owner, a matching tag, and uncommitted worktree changes do not replace that evidence. Missing identity, missing files, unreadable storage, and incomplete native records are reported as unavailable.

Native transcripts and committed LOG content are compared using the same snapshot of the repository's existing secret mappings. Missing mappings for protected content make the comparison unavailable. A transcript created by `resume` is compared with its own recorded byte baseline and digest, because another runtime can remint the restored history. The report distinguishes unchanged content, appended content, truncation, and rewriting. Missing or changed branch-tip evidence is reported separately from baseline integrity. Superseded claims are excluded.

These comparisons do not learn secret mappings, write dictionary files, or alter claims. Before repairing a truncated or rewritten transcript, inspect the explicit recorded branch and the native transcript, then choose an explicit import or fork.

The sign-in row checks the selected Hub's local access and refresh expiry independently. Expired access with an unexpired refresh token can be renewed by a later authenticated request. An expired refresh token prevents renewal even while access has not expired. Unreadable expiry timestamps are reported as unknown. Local expiry is not proof that the Hub still accepts a credential; `agit whoami --check` requests that online verification explicitly. The local expiry check does not refresh credentials or send a token.

Local merge diagnostics inspect the selected repositories' shared Git-directory transaction records without modifying them. They distinguish an open transaction, a missing or moved target, an unavailable or moved local source, malformed records, and inspection limits. A historical source selector or a source in another repository is explicitly qualified when it is not re-evaluated. Transaction age and leftover lock filenames do not establish staleness. Valid records receive an explicit `merge --into ... --status` command; malformed records must be preserved for recovery because the existing merge commands cannot deserialize them either. Doctor never aborts a merge or deletes a lock.

`--deep` freezes local branch, remote-tracking, tag, and HEAD references to commit IDs, then checks their reachable history including merge parents. Each committed session snapshot is validated using its own layout, LOG, event objects, and VIEW. File-line snapshots and ancestry before any line declaration are identified separately. A healthy tip or dirty worktree cannot hide a damaged historical VIEW. Findings name the exact commit and a corresponding frozen ref.

History inspection reads local objects without fetching missing promisor data or honoring replacement objects. Missing objects, unavailable ancestry, invalid refs, and resource limits produce an explicit incomplete report. Work is bounded by root count, commit count, object reads, object bytes, expanded sequence size, and retained findings; reaching a bound is not proof that the remaining history is sound. This checks reachable AgentGit snapshots, not unreachable Git objects or reflog-only history. Ordinary committed snapshot reads use the same bounded local object reader.

Older Git versions remain supported through checked worktree registration records. If a registered path contains line breaks that their output cannot describe unambiguously, inspection reports unavailable; use Git 2.36 or newer to inspect such paths. Duplicate branch registrations retain the primary worktree's diagnostic priority.

The `secret keystore` row probes the store `secrets.keystore` selects the way a commit uses it (the OS credential store gets a throwaway entry written, read back and deleted) and unlocks the vault if one exists. A warning there means `agit secrets add` and any commit that finds a secret fail; on a machine with no desktop session, `agit config secrets.keystore file` keeps the key in a private file instead.
