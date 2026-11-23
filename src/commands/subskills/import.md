---
name: agit-import
description: Adopt an existing runtime transcript into an Agent repo session branch.
---

# agit import

## Purpose

Import an existing Codex, Claude Code, or other runtime session into AgentGit. Import creates or binds a real `refs/heads/<branch>` and stores the transcript as context; a workspace binding does not select the correct repo automatically.

On a TTY, the zero-argument form opens the naming inbox for unmanaged sessions.
Select a session, choose its destination repo, and enter a branch name; the
screen leaves the alternate buffer before a separate lineage choice. Choose a
verified candidate, import independently, or cancel. Cancel is selected initially,
even when there is only one candidate. Explicit `--onto`, `--independent`, and
`--link-only` decisions use their existing claim and settlement checks.

## Synopsis

```bash
agit import [session] --into <owner/name@branch> [options]
agit import [session] --repo <owner/name> -b <branch> [options]
```

## Options

| Option | Meaning |
|---|---|
| `[session]` | Explicit native runtime session ID or unique ID prefix from runtime discovery; omitted targets require a choice in the interactive picker. Arbitrary transcript paths and `@` do not select the current runtime session |
| `-n, --name <agent>` | Repo name only when a new Agent repo must be created |
| `--from <runtime>` | Source runtime, such as `codex` or `claude-code` |
| `--link-only` | Write the adoption link without importing/settling content |
| `--into <owner/name>@<branch>` | Explicit destination Agent repo and session branch |
| `--repo <owner/name>` | Compatibility alias for `--into`; supply `-b` separately when the value omits the branch |
| `-b, --branch <branch>` | Target session branch; recommended explicitly when creating/importing |
| `--propose-lineage` | Read-only local prefix report; requires a full native ID, `--from`, and qualified destination branch |
| `--independent` | Explicitly use the ordinary no-base import path; existing claim and settlement checks still apply |
| `--onto <ref>` | Attach a new branch at an existing ref; an existing branch must retain that ref in its first-parent history |
| `--privacy` | Adopt a privacy-redacted transcript copy (currently Claude Code only) |
| `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color` | Common options; global `--json` emits the unified CLI JSON envelope |
| `--tui` / `--no-tui` | Force or forbid the full-screen interface; machine-output flags still forbid it |

## Scenarios and examples

Use `--into <owner/repo>@<branch>`, or `--repo <owner/repo> -b <branch>`, to select the destination. Do not combine a branch in `--into` with `-b`. If the session ID is unknown, start with `agit status --check-missing` or filter with `--from`. Use `--link-only` when adoption should be recorded but content should wait.

After `--link-only`, sign in and run `agit import <session-id> --from <runtime> --into <owner/repo>@<branch>`
to record the opening version. The offline link has no repository owner or branch claim;
`agit commit` cannot choose them. Session turns cannot be settled onto `main`.

```bash
agit import 132bf69f-22ab-4000-8000-000000000001 --from claude-code --repo szh/p1 -b fix-auth
agit import 132bf69f-22a --into szh/p1@imported --onto main
agit import 132bf69f-22a --link-only
agit import 132bf69f-22a --into szh/p1@pending --independent
```

After import, verify the real repository rather than trusting the summary:

```bash
git -C "$(agit repo path szh/p1)" show-ref --verify refs/heads/fix-auth
agit status
```

When a runtime session variable names the current conversation (for example
`CODEBUDDY_SESSION_ID` in WorkBuddy), the candidate list and the empty-list message
say which conversation that is and, when it is already saved, the `agit commit` and
`agit push` commands for its target. Importing a session of the same runtime other
than that conversation prints a warning naming both native IDs (a `warning` row in
JSON `diagnostics.stderr`) and continues: nested runtimes and deliberate imports of an
earlier conversation are legitimate. Such an import is saved locally but not pushed
automatically, even with `push.auto` on; recheck the ID, then publish it with
`agit push <owner>/<repo>@<branch>`.

`--name` names a repo; it does not create a branch by itself. Prefer the qualified `--into` form when reusing a repository, especially in an organization namespace. Import does not change the parent shell's environment: subsequent commands need a full target or an explicitly set `AGIT_SESSION`.

## Inspect possible local lineage

Before adopting a session, inspect verified local prefixes:

```bash
agit import 132bf69f-22ab-4000-8000-000000000001 --from claude-code --into szh/p1@imported --propose-lineage --json
```

Preview requires the full native ID and an explicit destination branch. It does
not infer either identity from the cwd or `AGIT_SESSION`. It reads bounded native
bytes and existing local history without creating a repository, adoption link,
export cache, dictionary, or credential file, and without fetching objects.
OpenCode inspection uses a coherent read-only database transaction; SQLite can
maintain its ordinary WAL coordination sidecars without modifying application
rows. Unsupported or incomplete evidence is reported explicitly.

Inspecting an existing repository requires NUL-framed Git worktree output,
normally available in Git 2.36 or newer. Unsupported output reports
`git_worktree_format`; it is never parsed as ambiguous newline-delimited paths.
This capability requirement is local to discovery and selection. Explicit
`--onto`, `--independent`, and `--link-only` imports retain their ordinary checks.

A candidate command supplies `--onto` with the frozen commit. Choose a candidate
explicitly, or use the returned `--independent` command to import without choosing
a prior session base. Neither a singleton candidate nor an empty report selects
an action. Semantic comparison is unavailable, so no exact candidate does not
prove an unknown starting point.

Text actions preserve the selected directory, AgentGit home, and Hub. Unix output
uses POSIX shell syntax; Windows output starts a child PowerShell process and
requires PowerShell 7.3 or newer. The child preserves literal arguments and leaves
the parent shell's routing unchanged. Use `--json` when an argument contains a
terminal control character that cannot be displayed as a safe command.

For an explicit, self-contained Codex rollout larger than 64 MiB, the report streams the complete file. The limits are 512 MiB per file, 8 MiB per record and 262,144 records. It retains at most 16 MiB of complete records plus record hashes. Paginated histories requiring a parent, materialized baselines outside that prefix and suffix records requiring secret mapping can still return incomplete. An incomplete scan never proves the absence of earlier history.

`--propose-lineage` conflicts with `--onto`, `--independent`, `--link-only`,
`--privacy`, and `--name`. `--independent` conflicts with `--onto` and `--link-only`.
Versioned `--privacy` also requires `--onto` or `--independent`, so a preview cannot
create a redacted copy. Explicit `--link-only --privacy` keeps its offline copy-and-link
behavior.

## Choose before adoption

With `--link-only` or `--independent`, a native ID or prefix that matches no
session returns exit code `3`. An ambiguous prefix requires selection and returns
exit code `8`. Listing without an ID succeeds when there are no unadopted
sessions. Human and JSON output use the same exit codes.

The legacy `-n <name>` form requires a unique local checkout. Competing repo or
`--onto` candidates return `8` without a terminal; a missing `--onto` base returns
`3`. These checks precede adoption and privacy-copy creation. Once selected, the
repo path and base commit are retained rather than resolved again from moving names.

`--onto` selects a whole commit in that destination repository. A qualified ref must
name the same repo; a bare repo qualifier must be locally unique. `@` requires an
`AGIT_SESSION` in the same repo. Commit ancestry (`~n`) and a single turn (`#n`) are
supported; event, range, and file selectors are rejected rather than widened to a
whole commit.

Without an explicit lineage decision, supply the full native ID, `--from`, and a
qualified destination branch. A call without an ID can list unmanaged native
identities and an explicit retry command; it does not adopt a discovered row. The command inspects only local evidence before
asking. Pipes, JSON, CI, agent sessions, quiet mode, and `--yes` cannot answer this
choice: they return exit code `8` with `operation: "choice_required"` and explicit
candidate and independent actions. The v2 envelope also carries these actions in
`fix`; v1 keeps its existing envelope. Running an action is an explicit new import,
subject to the ordinary current permission and lineage checks.

Interactive acceptance rereads the native source, original claim image, selected
repository, and destination ref. It repeats these checks under the branch and
claim locks before publishing. A changed observation refuses the choice and asks
for another inspection. A selected candidate remains its immutable commit even
if its displayed aliases move; its evidence must still match. External native
writers are not locked, so ordinary settlement checks still govern later changes.

Repeating an import into the same active, explicitly selected claim keeps the
ordinary idempotent settlement path. No hidden runtime or directory identity is
used to choose a destination.

## Adopt the current session when it is not managed yet

For a Claude Code or Codex conversation use `agit save` (SKILL.md). For other runtimes, or when
the user names an explicit repository and branch, uploading the current conversation takes two
commands. Do not run `agit status --json`, `whoami`, `login`, `doctor`, `log`,
`push --dry-run` or `--help` before either: save and push check the sign-in and show the
destination, count and visibility themselves.

- Already saved (`agit status` names its `<owner/repo>@<branch>`): `agit commit
  <owner/repo>@<branch>`, then `agit push <owner/repo>@<branch> --yes`.
- Not saved yet: `agit import <session-id> --from <runtime> --repo <owner/repo> -b <branch>
  --independent`, then `agit push <owner/repo>@<branch> --private --yes`. The repo is the
  one the user named, otherwise the `bound repo` from one plain `agit status`. Use
  `--independent` only when this conversation does not continue a saved session; when
  it does, pass `--onto <ref>` instead.

Push only when the user asked to upload. Always pass `--private` for a new repository,
and `--public` only when the user says public. The details below cover other runtimes,
an unknown native ID and the lineage choice.

When the user asks to upload, save, or adopt the current conversation and its native session ID is needed, find the session below. An absent `AGIT_SESSION` alone is not a reason to scan or adopt transcripts. Do not use `agit new` to upload the running conversation:

```bash
agit status --check-missing
```

This reports the resolved identity, if any, and scans the runtime directories for sessions that no Agent repo has adopted yet. The current transcript may already be adopted even when `AGIT_SESSION` is absent. Match its native session ID from the runtime or hook against the reported metadata; never choose by recency alone.

Status also reports the conversation the runtime names (`runtime_session` in JSON): its runtime, native ID, the variable it came from, and the target it is already saved to. It is display only and never selects a command target. In WorkBuddy the current native ID is `$CODEBUDDY_SESSION_ID`; in Claude Code it is `$CLAUDE_CODE_SESSION_ID`. When the current conversation is already saved, do not import anything: when the user asks to save it, settle its new turns with `agit commit <owner/repo>@<branch>` using the reported target, and publish with `agit push <owner/repo>@<branch>` only when the user asks to. Candidate lists leave out every saved session, the current one included, so never import a different session because it is the only unsaved candidate. An import that names another session of the same runtime prints a warning with both IDs and is not pushed automatically; recheck the ID before publishing it with `agit push`. When the variables name several conversations (a nested runtime), ask the user which one to save.

When the user asks to upload, save, or adopt the current session and it is not saved yet, explicitly pass its native session ID (or choose it in the interactive import picker) and adopt it into the repo chosen by the rules in [new](new.md):

```bash
agit import <session-id> --from <runtime> --repo <owner/repo> -b <branch>
```

`agit import` links that existing transcript to a real session branch and records its first version after an explicit lineage choice. Use the full native ID and `--from` runtime. A terminal offers verified bases, independent import, or cancellation; pipes, JSON, CI and agent calls return choices without adopting. Pass `--onto <ref>` or `--independent` to make the decision explicitly in automation. `--propose-lineage` only inspects local evidence. `agit new` cannot take over the session that is already running: it launches a different session with an empty VIEW. Use `new` only when the user explicitly asks to start a fresh session. `-n <agent-name>` is only for naming a new Agent repo when none can be reused; it does not pick the branch.

Import cannot change the calling process's environment. Keep using the selected
`<owner/repo>@<branch>` explicitly for later commands, or supply that exact
`AGIT_SESSION` to each invocation. To inspect saved context without preparing a
runtime, use `show` or `view`. `resume --no-launch --json` prepares a runtime
session and records its claim; it is not a read-only preview.

### New Antigravity CLI text conversations

From a checkout of the AgentGit source, run `python3 scripts/capture-antigravity-cli.py --prompts /path/to/prompts.ndjson` in the intended workspace (use an absolute script path when running elsewhere). This starts a **new** signed-in `agy` process and sends each supplied prompt to Antigravity. It never searches existing conversations. The input file contains one JSON object per line, for example `{"event":"user","message":{"content":"Reply with the word amber."}}`. The helper requires Python 3 and Antigravity CLI with headless streaming input/output (verified with CLI 1.5.14).

On success it prints the full conversation ID and stores a text-only projection in `$AGIT_HOME/captures/antigravity-cli/` (default `~/.agit/captures/antigravity-cli/`). Use `agit import <full-id> --from antigravity-cli --independent --into owner/repo@branch` to record it. The capture deliberately omits runtime configuration, internal steps, attachments and usage; tool conversations and unfinished streams are not supported. Neither old histories, App/IDE exports, nor native resume are supported. The helper does not publish to the Hub; import uses the destination's ordinary push policy.
