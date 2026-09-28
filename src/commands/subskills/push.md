---
name: agit-push
description: Publish existing local Agent repo refs to the Hub; it never creates a session branch.
---

# agit push

## Purpose

Publish refs that already exist locally. `push` is not a replacement for `new`, `fork`, or `import`, and it does not promise an automatic push for every turn.

## Synopsis

```bash
agit push [owner/repo@branch] [options]
```

## Options

| Option | Meaning |
|---|---|
| `[owner/repo@branch]` | Explicit saved branch; a bare repo with `-b` is also accepted. An omitted target requires `AGIT_SESSION` outside the terminal picker |
| `-b, --branch <branch>` | Branches to publish; repeatable |
| `--all` | Publish all local branches/refs |
| `--private` | Use private visibility when creating the destination |
| `--public` | Use public visibility when creating the destination |
| `--allow-secrets` | Explicitly accept complete deterministic credential findings for this push |
| `--audit` | Open an interactive sensitivity reviewer for the frozen outgoing publication, then ask separately before publishing |
| `--dry-run` | Scan and show the plan without uploading |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |

Visibility is settled once, at first publish. Without `--private` / `--public`, push takes the repo preference recorded by `agit init --private`, then the global `push.visibility` (`public` or `private`; `ask` means ask), and otherwise asks on a TTY (non-interactive runs default to private). `--dry-run` prints which of these applies.

A bare `agit push` in a human terminal selects a saved session in the TUI. The
choice applies only to this invocation. Agent and script callers use a complete
target or `AGIT_SESSION`; workspace bindings, native IDs, and the newest session
do not choose what to publish. `--yes` does not select a missing target.

## Review secret protection before pushing

Before an authorized push, complete this review using the selected Agent repo, not the source-code repository:

1. Inspect `agit secrets review --repo <agent-repo-path> --json` and run `agit scan <owner/repo>@<branch> --secrets --json`. Review existing protection candidates even when the scan is clean: already protected values appear as placeholders and may include false positives.
2. Check the actual values in authorized local source content and assess them for the requested audience. `review` returns opaque IDs and policy metadata, not plaintext; a label or placeholder alone is insufficient evidence for `allow`. Keep real credentials and uncertain candidates protected.
3. For each confirmed false positive, run `agit secrets allow <record-id> --repo <agent-repo-path> --reason <reason>`. When supplying the exact value directly, pipe it into `agit secrets allow --stdin --repo <agent-repo-path> --reason <reason>`. See [secrets](secrets.md) for input limits and structured local/synchronization results.
4. Rescan after each batch of allowance changes and inspect synchronization status, then proceed with the original push target and options. A truncated or sampled report is not the full finding set; finish reviewing remaining findings before publishing. Keep genuine credentials protected throughout this workflow.

Declarations synchronize immediately when possible and again before publication. Offline decisions remain pending; check their status before relying on server policy. Ordinary push refreshes acknowledged policy and completes pending synchronization before LFS, branch or tag uploads. First publication may create the selected repository before synchronization. An older Hub without the protocol reports a missing capability. `--dry-run` checks and reports pending synchronization without remote writes. A clean local scan does not acknowledge server policy.

Use exact `allow` decisions for routine false positives. When publication is already authorized and you have determined that all remaining findings are suitable for that audience, you may use `--allow-secrets` yourself. Explain the classification and report use of the flag. The review above is the agent's responsibility within the existing publication authorization; a scanner finding alone does not require another user confirmation. Unexamined findings in a bounded report still require review; incomplete scans, missing objects, integrity failures, authorization errors and other publication errors need their own diagnosis.

`--allow-secrets` applies only to this command's branch and version-tag requests, including verified LFS content. It emits a warning and reports any declarations whose synchronization did not complete. An ordinary retry does not inherit acceptance. Hiding an event from VIEW leaves LOG and Git history available for publication; it does not clear a historical credential finding.

## Interactive publication review

Ordinary push runs the deterministic secret scan. Add `--audit` to review disclosure
risks that depend on meaning and context, such as private conversations or internal
customer information. This review includes the complete historical LOG, historical
shared files, commit and tag messages, and readable LFS payloads in the selected
publication. Material hidden from VIEW still belongs to LOG and may be published.

```bash
agit config runtime.default claude-code
agit push alice/payments@session-1 --audit
```

The reviewer opens in the same terminal with visible progress and interactive
questions. It uses immutable `agit show audit/source@<full-commit> --log-only --raw
--no-tui` reads in an isolated inspection store and bounded exports of the complete
published carriers. The native runtime uses its configured model provider; the
review can send the supplied content to that provider. It requires an installed
Claude Code runtime with the required controls, and `runtime.default` must select
`claude-code`. Other configured runtimes are refused explicitly.

When the report is ready, exit the reviewer normally to return to push. Push checks
the report and displays findings before asking for a separate publication decision.
Answers inside the reviewer and global `--yes` do not replace that final decision.
The complete JSON report remains available during this confirmation. Temporary
inspection files are removed when the push command ends.

Incomplete review, unreadable evidence, unsupported decoding, exhausted budgets,
unanswered required questions, or an interrupted reviewer stop the audited push.
Verified binary content is listed as excluded from text review. Model findings are
advisory; the deterministic secret gate and explicit `--allow-secrets` policy still
apply. The reviewer does not edit, redact, create repositories, or publish content.

If the destination does not exist when review starts, the final decision authorizes
ensuring that named repository exists with the reviewed audience. A matching
repository created meanwhile must pass fresh identity and write-access checks.
When taking ownership of a read-only checkout, audited push relocates the local
checkout and publishes only the reviewed refs and captured LFS payloads; it does
not ask the server to copy the source repository's other history.

`--audit --dry-run` performs the interactive review and prints its report without
creating, promoting, or publishing a repository. Audited push requires terminal
input and output and cannot run with `--json` or from an unattended script.

## Examples

```bash
agit push szh/p1@feature-a
agit push szh/p1 --all
agit push szh/p1 -b feature-a --dry-run
```

A secret scan runs before publishing. If `refs/heads/<branch>` is missing, create it with `new`, `import`, or `fork` and verify it first. Push does not create a repo or branch from cwd.
