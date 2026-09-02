---
name: agit-secrets
description: Register device-local literal secrets and review the repository-local protection policy.
---

# agit secrets

## Purpose

Two layers of protection, both device-local. The **vault** holds literals you register explicitly — low-entropy values the heuristic rules would never catch on their own ("blue horse battery", an internal hostname). The **repository dictionary** holds what the heuristic rules found in session content by themselves; `agit commit` projects both into opaque `{{AGIT_SECRET_V1:...}}` placeholders before Git object ids are formed, and only this device can hydrate them back.

Secrets never travel through argv: interactive input is hidden, automation must pass `--stdin`. There is no show/decrypt/export path — `list`, `status` and `review` only ever print opaque ids and the labels you chose.

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
| `allow <record-id>` | Stop projecting a heuristic candidate from now on | `--repo <path>` |
| `unallow <record-id>` | Restore default protection for an allowed candidate | `--repo <path>` |
| `block add <name>` | Add an exact repository-local block rule | `--stdin`, `--allow-short`, `--repo <path>` |
| `block remove <record-id>` | Clear the explicit block bit | `--repo <path>` |

## Examples

```bash
agit secrets add staging-db-password
printf %s "$TOKEN" | agit secrets add ci-token --stdin
agit secrets list
agit secrets status --json
agit secrets review
agit secrets allow sec_2f3a...
agit secrets block add prod-hostname --stdin
agit secrets remove ci-token --yes
```

## Notes

`--allow-short` accepts a 4–7 byte rule. A short rule matches everywhere and materially raises both false positives and enumeration risk; prefer a longer literal when one exists.

`allow` only changes *future* projection. The reverse mapping is retained so placeholders already written into history keep hydrating on this device. An explicit `block` always wins over a heuristic `allow`, and neither registered rules nor block rules honour the store's `.agit-allow-secrets` allowlist or inline pragmas — a repository's contents cannot switch off a policy you set on your own device.

`remove` is irreversible: placeholders written under that record can no longer be hydrated anywhere. Unregister a value only when it is no longer a secret.

Both vaults keep their key in the keystore `agit config secrets.keystore` selects: `os` (the default) is the system credential store — macOS Keychain, Windows Credential Manager, the Secret Service on other Unix systems; `file` is a private file under `AGIT_HOME/keystore/`, for a machine with no desktop session (an SSH login, a CI runner) where no Secret Service answers — Unix only, protected by its file mode alone. With `file`, a backup of `AGIT_HOME` carries the global vault together with every key; the repository dictionary lives in the checkout's `.git`, so decrypting it takes a backup that holds both the repository and `AGIT_HOME`. The choice is per machine and nothing falls through from one store to the other; `agit doctor` reports whether the chosen store answers. When it does not, `add` and any commit that finds a secret fail closed rather than write an unprotected vault.

Projection happens at `agit commit`, not during transport — rewriting bytes at `git push` would change object ids and make local and remote history disagree. `agit push` stays a repository-wide fail-closed residue check: it refuses to publish when a protected literal survives in any object it would send.
