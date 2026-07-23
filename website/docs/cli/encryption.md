---
sidebar_position: 8
title: Encryption
---

# Encryption

Encryption keeps session content unreadable at rest. The store commits ciphertext, and only enrolled
recipients can decrypt it. This is separate from the [signing key](./identity.md): the signing key proves
who produced a session (provenance); encryption controls who can read one. Different keys, different jobs.

## The keybox

agit encrypts each session's content under a per-session symmetric **content key (CK)**. The CK is never
committed in the clear. Instead the store commits a **keybox** (`.agit/keybox.jsonl`) that holds one
sealed envelope of the CK per recipient. Each envelope wraps the CK to a recipient's **X25519** public
key: a fresh ephemeral keypair, an ECDH against the recipient's key, an HKDF-SHA256, and an
XChaCha20-Poly1305 seal. Only the holder of the matching X25519 secret can recompute the shared secret and
open the CK.

The transcripts themselves are sealed and unsealed by a git clean/smudge filter, so the working tree is
plaintext for you while everything committed and pushed is ciphertext.

## Enable encryption

```bash
agit a encrypt
```

With no flags this is the zero-config path: it mints a per-session content key into a repo-local keyring
and commits a keybox whose default recipient set follows the session's owning scope (team-readable under
an org, explicit for a personal owner). Name recipients directly:

| Invocation | Recipients |
|---|---|
| `agit a encrypt --readers alice,bob` | The named hub accounts. |
| `agit a encrypt --public` | Anyone (a public stanza; readable by all). |
| `agit a encrypt --team` | The owning org's current Team KEK. Combine with `--readers`/`--public`. |
| `agit a encrypt --team --org <org>` | Name the org explicitly. |

`--yes` (`-y`) confirms non-interactively.

## Enroll teammates as recipients

A recipient's X25519 public key is the encryption half of their [hub identity](./identity.md), enrolled
alongside their ed25519 signing key. Once a teammate has enrolled with `agit identity register`, add them
to a session's keybox:

```bash
agit a readers add alice
agit a readers add --public
agit a readers add --team
```

Adding a reader is an O(1) keybox append: it wraps the existing CK to the new recipient's key, no
re-encryption of the transcripts. Removing one is a real rotation, because a removed reader still holds
the old CK:

```bash
agit a readers rm alice
agit a readers ls          # (also the bare `agit a readers`)
```

`agit a readers add` takes `--key HEX` to supply a recipient key directly and `--repin` to accept a
changed registered key.

## Rotate the content key

```bash
agit a rekey
```

`agit a rekey` mints a new content key, re-seals it to the current recipient set (preserving a public
stanza if one was set), and rotates the keyring. Use it after removing a reader, or on a schedule.

## Recover keys on a new machine

When you clone or pull a store whose keybox includes you, recover this machine's content keys from the
committed keybox into the repo-local keyring so the filter can decrypt:

```bash
agit crypt unlock
```

This opens the envelope sealed to your X25519 key and writes the recovered content keys into the local
keyring. It resolves the active agent itself.

## Scrub pre-encryption plaintext

Enabling encryption seals going-forward blobs, but earlier commits still hold the plaintext. Rewrite it
out of history:

```bash
agit a purge-history
```

`agit a purge-history` re-encrypts every historical revision of `sessions/**` under the current keyring so
no pre-encryption plaintext survives in any commit. It is guard-railed: it checks per-session
preconditions, requires a clean tree, uses `git filter-repo` when available (falling back to `git
filter-branch`), and never auto-pushes. It prints the exact force-push command to run after you review the
rewrite. `agit a encrypt --purge-history` is an alias for the same command.

## Machine-global key (no-hub setups)

For a store with no hub, `agit a encrypt` also supports a single machine-global symmetric key you share
out of band:

| Invocation | Effect |
|---|---|
| `agit a encrypt --export <file>` | Export the key to a file to share with a teammate. |
| `agit a encrypt --import <keyfile>` | Import a shared key. |
| `agit a encrypt --rotate` | Mint a new current key and re-encrypt the working tree; retired keys stay local so history still decrypts. |

After a rotate you must `agit a push` and re-share the new key with `--export`, then `agit a
purge-history` if you need pre-rotation blobs gone.
