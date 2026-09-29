---
name: agit-secrets
description: Register device-local literal secrets and review the repository-local protection policy.
---

# agit secrets

## Purpose

Two layers of local protection. The **vault** holds literals you register explicitly — low-entropy values the heuristic rules would never catch on their own ("blue horse battery", an internal hostname). The **repository dictionary** holds what the heuristic rules found in session content by themselves; `agit commit` projects both into opaque `{{AGIT_SECRET_V1:...}}` placeholders before Git object ids are formed. These local handles require their dictionary to hydrate. Privacy publication encrypts the recovered session text and its used protection values for the viewing key; `agit privacy unlock` restores the text on another device and installs local protection for those values. It does not transfer the full dictionary or its keys.

Values never travel through argv: interactive registration is hidden, and automation passes `--stdin`. Management output contains record IDs, labels and policy metadata. It never contains the stored value or its digest.

## Synopsis

```bash
agit secrets <subcommand>
```

## Subcommands

| Subcommand | Purpose | Main options |
|---|---|---|
| `add <name>` | Register a literal in the device-local vault | `--stdin` reads one secret from stdin; `--allow-short` permits a 4–7 byte rule |
| `list` | List opaque ids and labels | `--json` |
| `remove <id-or-name>` | Delete one record irreversibly | `--yes` skips the prompt |
| `status` | Authenticate the vault and every encrypted record | `--json` |
| `review` | Review this repository's candidate policy | `--repo <path>`, `--json` |
| `allow <record-id>` or `allow --stdin` | Declare an exact repository value non-secret | `--repo <path>`, `--reason <text>`, `--json` |
| `unallow <record-id>` | Revoke a declaration and restore normal detection or prior protection | `--repo <path>`, `--json` |
| `block add <name>` | Add an exact repository-local block rule | `--stdin`, `--allow-short`, `--repo <path>` |
| `block remove <record-id>` | Clear the explicit block bit | `--repo <path>` |

## Examples

```bash
agit secrets add staging-db-password
printf %s "$TOKEN" | agit secrets add ci-token --stdin
agit secrets list
agit secrets status --json
agit secrets review
agit secrets allow sec_2f3a... --reason "Public identifier"
printf %s "$VALUE" | agit secrets allow --stdin --repo /path/to/agent-repo --reason "Test fixture"
agit secrets unallow sec_2f3a... --repo /path/to/agent-repo --json
agit secrets block add prod-hostname --stdin
agit secrets remove ci-token --yes
```

## Notes

`--allow-short` accepts a 4–7 byte rule. A short rule matches everywhere and materially raises both false positives and enumeration risk; prefer a longer literal when one exists.

`allow` exempts the complete detector-matched value from future local projection and client scans, even when a global registration or repository `block` also matches. It does not exempt a distinct overlapping credential, a containing value, a rule or a file. JSON escapes are decoded before semantic matching. The same value in the same dictionary reuses its record ID, whether supplied by ID or stdin. Old placeholders still hydrate.

Choose exactly one of `<record-id>` and `--stdin`. Stdin accepts 1 to 65,536 UTF-8 bytes, removing one final LF or CRLF and preserving other whitespace. The optional reason accepts up to 1,024 UTF-8 bytes; keep it descriptive and free of credentials. `unallow` restores the original protection sources. A value introduced only as a declaration returns to normal detection.

Local intent is saved first, then the CLI immediately attempts synchronization with the current login and immutable repository target. Offline failures, missing capability and conflicts leave a durable pending operation and return nonzero while explicitly reporting local completion. JSON includes `local_applied`, the saved `record` and `synchronization`; `review --json` exposes `local_state`, `sync_status`, `pending_operation`, `target`, `server_policy_id`, `server_version` and `write_outcome_uncertain`. `synced` means the Hub acknowledged the decision; it does not mean refs were published. If an earlier write timed out without confirmation, an opposite decision remains pending until synchronization can establish that the earlier request cannot supersede it.

Ordinary push refreshes confirmed policy and synchronizes pending operations before LFS or Git uploads. First publication can create the selected repository and bind its immutable ID before synchronizing. A Hub or repository identity change cannot transfer declarations. A remote revocation or version conflict never silently reactivates an old allowance. Inspect the current state, then issue a new `allow` or `unallow` decision to resolve a conflict. Old local allowances receive the same tombstone check on their first synchronization.

`--dry-run` checks and reports planned synchronization without mutating remote policy or creating a repository. If remote policy has changed since the cached scan policy, it reports that a normal push must refresh it. An older Hub without the protocol blocks ordinary push when declarations need synchronization. An explicitly reviewed `--allow-secrets` push retains its operation-wide path and warning, reports any unsynchronized declarations and leaves them pending. Exact declarations never enable that flag automatically.

The device's `$AGIT_HOME/.agit-allow-secrets` file and built-in exemptions remain local policy; they are not queued as repository declarations. Inline pragmas do not override registered rules.

`remove` is irreversible: placeholders written under that record can no longer be hydrated anywhere. Unregister a value only when it is no longer a secret.

Only the global registration vault uses `agit config secrets.keystore`: `os` (default)
selects the system credential store; `file` selects a private file under
`AGIT_HOME/keystore/` (Unix only). Repository dictionaries automatically keep their key
under their common Git directory at `agit/secret-dictionary/keys/`, alongside the encrypted
mapping. They do not depend on the global setting or create system credential entries.
Back up the entire repository dictionary directory to preserve local hydration, and treat
that backup as sensitive. Git push never uploads it.

An existing dictionary may need its previously selected keystore once. After authenticating
all records, a locked operation installs a local key and atomically records the new storage
route without changing placeholders. Strict read-only inspection does not migrate. A missing
local key after migration is an error, not permission to fall back to a system credential.

On macOS, a noninteractive command cannot open a Keychain authorization dialog. If Keychain requires authorization, have the user rerun the command from a terminal in their macOS login session and approve access, then retry automation. For global secrets or a dictionary awaiting migration, keep the previous keystore available. Choose "Always Allow" to retain access for the same signed executable; "Allow" grants one access. A rebuilt ad-hoc-signed binary may need authorization again. Migrated dictionaries do not use Keychain.

Projection happens at `agit commit`, not during transport — rewriting bytes at `git push` would change object ids and make local and remote history disagree. `agit push` stays a repository-wide fail-closed residue check: it refuses to publish when a protected literal survives in any object it would send.
