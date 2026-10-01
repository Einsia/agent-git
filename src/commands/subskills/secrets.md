---
name: agit-secrets
description: Manage global and repository privacy policy with reversible cloud-synchronized mappings.
---

# agit secrets

## Purpose

Local scanning automatically replaces detected secrets with opaque `{{AGIT_SECRET_V2:...}}` placeholders. A durable dictionary retains exact originals for recovery. Default global rules, global user rules and repository user rules keep their scopes; explicit blocks override allows, and allows suppress heuristics.

Secrets never travel through argv: interactive input is hidden, automation must pass `--stdin`. There is no show/decrypt/export path — `list`, `status` and `review` only ever print opaque ids and the labels you chose.

## Synopsis

```bash
agit secrets <subcommand>
```

## Subcommands

| Subcommand | Purpose | Main options |
|---|---|---|
| `add <name>` | Register a global literal block | `--stdin` reads one secret from stdin; `--allow-short` permits a 4–7 byte rule |
| `list` | List opaque ids and labels | `--json` |
| `remove <id-or-name>` | Disable an explicit global block; retain recovery data | `--yes` skips the prompt |
| `status` | Show local dictionary and key availability | `--json` |
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
agit secrets allow RECORD_ID
agit secrets block add prod-hostname --stdin
agit secrets remove ci-token --yes
```

## Notes

`--allow-short` accepts a 4–7 byte rule. A short rule matches everywhere and materially raises both false positives and enumeration risk; prefer a longer literal when one exists.

`allow` only changes *future* projection. The reverse mapping is retained so placeholders already written into history keep hydrating on this device. An explicit `block` always wins over a heuristic `allow`, and neither registered rules nor block rules honour the store's `.agit-allow-secrets` allowlist or inline pragmas — a repository's contents cannot switch off a policy you set on your own device.

`remove` and `allow` change future policy without deleting recovery records or rewriting existing commits. Dictionary membership alone is not an explicit block.

Cloud account keys are generated automatically after login and cached locally. No user keystore configuration is required. If a key is unavailable, pending originals are stored temporarily in the private local journal. Every push, including a no-op push, independently attempts encrypted dictionary synchronization. Plaintext dictionaries are never uploaded. Another device signed in to the same account downloads the encrypted packages and restores placeholders locally. Failed synchronization is retried on later synchronization triggers; unresolved placeholders remain readable until recovery data arrives.

Processing runs in a bounded isolated local worker. Any failure skips the failed privacy work and never blocks recording, upload, import, restore, or live output. LFS payloads are excluded. The server only stores keys, received commits, and encrypted packages; it does not scan or intercept for privacy. The legacy `secrets.keystore` setting only affects reading older vaults. Explicit management commands may report their own errors, but ordinary operations never depend on their success.
