---
name: agit-privacy
description: Choose initial session publication modes, configure privacy policy, and manage original access.
---

# Privacy policy and original access

New sessions default to plain publication. Existing sessions retain their recorded mode.
Encryption is opt-in before the first registration. Repository visibility and access to original content are separate:
key delivery requires current repository membership and permission for the exact saved snapshot.

Before a session is registered, its creator can explicitly enable encryption:

```sh
agit privacy mode alice/agent@work encrypted
agit push alice/agent@work --private
```

This local choice covers all unpublished snapshots and future commits of the logical session,
including its other branches. Change the mode only when authorized by the user.
`plain` restores the default before registration; omit the mode to inspect current settings.
The first registration locks the mode, including registration during an unsuccessful push or an
original-access policy operation. Repository visibility flags do not select encryption.
Plain content uses repository read access; local secret protection still applies.

Inspect exact publication storage and access without requesting a content key:

```sh
agit privacy status alice/agent@work
```

Status reports "Original saved encrypted; sanitized view pending" when ciphertext is stored but
display processing is incomplete. Display readiness and your original-read or grant-management
capabilities are separate fields. `--json` emits the validated publication state.

Copied snapshots keep the original repository's identity and access rules. A fresh or nested
copy resolves that source on the same Hub; source renames also preserve exact snapshot grants
and saved grant retries. Owning the copy does not grant access to its source's originals.
If the source is deleted or inaccessible, a fork can use sanitized context. Temporary source
or key-service failures stop the operation so they cannot silently select sanitized context.

Recover an encrypted snapshot on the machine that will run the session:

```sh
agit privacy unlock alice/agent@work --workspace /work/project
agit resume alice/agent@work --no-launch
```

Unlock uses the signed-in account to request the exact snapshot's content key, validates its
repository/session/commit/envelope bindings, and decrypts locally. The content key is not retained.
Recovery files live under the Git common directory's `agit/privacy-recovery/<commit>` with the
selected workspace. Resume preserves native messages, models and tool records within the same
runtime; cross-runtime conversion uses its ordinary loss warnings. Settlement retains recovered
LOG evidence without adding LOG-only conversation to the resumed VIEW.

Published encrypted snapshots are immutable. File, memory, VIEW and branch-seal edits require a
settled native continuation or an independent fork. Resume, continue the session, and commit the
new turn before editing, or use `agit fork` to create a new session with independent ownership.

The session owner can inspect and manage readers of an exact published snapshot:

```sh
agit privacy access show alice/agent@work
agit privacy access grant alice/agent@work
agit privacy access revoke alice/agent@work --member bob
agit privacy access stop alice/agent@work
agit privacy access retry alice/agent REQUEST_ID
```

Grant confirmation freezes the snapshot and current member list. It covers that snapshot's
complete original; historical and future snapshots require separate grants. Revoke removes one
reader's default and explicit access. Stop removes explicit grants while preserving eligible
default readers. Losing membership invalidates access; rejoining alone does not restore it.
Retry preserves the confirmed scope and revision. If another operation superseded it, inspect
current access before confirming a new request. See `docs/commands/privacy.md` for details.

`agit privacy` manages the policy used before a session, export, share, or push enters the
publication pipeline. The policy is stored below the local AgentGit repository's Git common
directory and is never committed to the AgentGit tree.

File collection requires an authorized workspace and an include match. Captured file-tool content
is visible by default without either requirement; structured file paths still honor explicit,
mandatory and sensitive-file exclusions. Memory collection remains opt-in.

Privacy projections default to `tools.omit: false` and `attachments: "original"`. To hide content:

```sh
agit privacy policy tool codex:exec_command --omit true --repo alice/agent
agit privacy policy tool mcp__company__query --omit true --repo alice/agent
agit privacy policy attachment metadata --repo alice/agent
```

Tool settings apply to both calls and results. A runtime-qualified exact name overrides a bare
exact name, which overrides the default. `tool --omit true` hides tools by default; a named
`--omit false` allows one. `tool NAME --reset` removes an override. Attachments support `omit`,
`metadata` and `original`; original retains recorded binary carriers and sanitizes readable text.
It does not fetch referenced files or URLs. Configurations without these fields use open defaults.
These settings affect encrypted public views, privacy exports and shares; plain push uses its
existing path. Existing accepted history is immutable. See `docs/commands/privacy.md` for details.

System/device mandatory exclusions override repository includes and branch settings. The CLI
loads the platform's system policy and the absolute source paths listed in
`$AGIT_HOME/privacy-policy-sources.json`. `privacy policy show` reports the effective rules, while
repository edits save only local authorization. Source versions and contents affect the policy
digest, which binds the prepared publication preview. Missing configured sources block
processing. See `docs/privacy-envelope.md` for paths and schemas; cloned policy text does not
install mandatory sources or authorize source-machine directories.

```bash
agit privacy policy set-workspace /work/project --repo alice/agent
agit privacy policy include 'src/**' --repo alice/agent
agit privacy policy allow-memory 'team.md' --repo alice/agent
agit privacy policy show --repo alice/agent
agit privacy preview --repo alice/agent --branch work src/main.rs --memory team.md
```

`privacy preview` evaluates only the paths supplied to the command. It does not recursively walk
an authorized directory. Every report includes a policy digest so a later publication step can
require a new preview when the strategy changes.

The publication path layer keeps a device-local alias table beside the policy file. `privacy preview`
records aliases as it evaluates candidates, so repeated previews use the same names. Authorized
roots use stable logical names such as `<workspace>/src/main.rs` and `<contracts>/contract.yaml`;
excluded or outside-root files use stable `<private-file-N>` placeholders. The reverse mapping is
never part of tracked repository content or a public preview.

The shared projection path aliases paths, applies configured replacements, and scans the rewritten
result before public bytes are produced. An excluded candidate produces no bytes, and an allowed
candidate can be sealed in the versioned private envelope used by later publication integrations.

For direct editing, `agit privacy policy file --repo alice/agent` prints the JSON path. The file
must retain the current policy version and is validated before it is used.
Its `metadata` setting defaults to `"minimal"`; `"project"` additionally publishes the sanitized
project origin and worktree summary. Push retains these processed fields in `metadata.privacy`
in both the public envelope and Git metadata. They are presentation facts, not local permissions
or workspace bindings.
