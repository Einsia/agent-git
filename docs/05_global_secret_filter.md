# RFC: Device-local global low-entropy secret filtering

Status: Implemented by the merge request that introduces this document.

## 1. The problem

`domain::secrets` recognizes known formats and high-entropy credentials with gitleaks rules.
It deliberately does not call `password = short` or a human-memorable phrase a secret: text
alone cannot separate an ordinary sentence from the user's own passphrase. A literal the user
registered explicitly carries no such ambiguity, yet still passes through the existing rules.

This RFC adds a **device-local, explicitly registered** set of secrets. It must satisfy:

- substring matching on any UTF-8 literal, with no dependence on format or entropy;
- against long transcripts and RC delta streams, scan cost grows linearly with the input
  instead of multiplying by the number of rules;
- user secrets never persist in plaintext;
- a leak of the database alone does not permit offline dictionary verification of a
  low-entropy secret;
- hit reports and logs never carry the original text;
- live RC, the publish gate and redacted copies share the same matching semantics.

## 2. Non-goals and the security boundary

This is not a vault against anyone who already holds local administrator rights, kernel access,
or read access to `agitd` process memory. The matcher has to hold the patterns at runtime; an
Aho–Corasick automaton is not a raw string array, but it can still be analyzed and the
patterns recovered, so it is protected at the same level as plaintext.

What this design protects against, with the OS credential store as the keystore (`os`, the
default; §3.2):

- a leak of the vault file, an ordinary backup or an offline disk on its own;
- the user copying or archiving `$AGIT_HOME` along with everything else by mistake;
- ordinary scan logs, CI output and the RC wire leaking matched content a second time.

With the file keystore (`file`) the first two do not hold. The key then sits under
`$AGIT_HOME` beside the vault, so a full backup of the home directory, an archive of it or an
offline disk holds both and decrypts the vault, unless the disk or the backup is itself
encrypted. The file keystore keeps the vault from other users of the same machine and from a
copy of the vault directory alone, and nothing more; the third item holds under either store.

What it does not promise:

- that secrets stay unreachable once the Secret Guard process is taken over;
- that malicious code holding the right to read the keystore — to unlock the OS keyring, or
  with the file keystore to read the user's files — cannot decrypt;
- that an authorized process with unlimited calls to the scan interface cannot guess online;
- that content another component wrote to a log or sent to the network before the filter was
  in the path can be recalled.

`agitd` is therefore the trust boundary: the main application and the hub get neither the
vault, the DEK, nor the plaintext rules.

## 3. Choices

### 3.1 An encrypted vault, not bare hashes

`SHA-256(secret)` over a low-entropy secret is an offline dictionary oracle. A per-record salt
plus Argon2 instead multiplies every session candidate by the number of rules, which cannot
carry high-volume arbitrary-substring scanning.

Storing only HMACs solves the database leak, but an arbitrary substring then needs an HMAC
over a sliding window at every registered length; with many distinct lengths that is still
`O(input × distinct_lengths)`. This feature takes a recoverable encrypted vault in exchange
for one build and a linear scan. The trade-off is explicit: holding the database and the
master key exposes every registered value immediately.

### 3.2 Envelope encryption

Keys come in two layers:

```text
KEK  256-bit, handed to the configured keystore (§3.2)
  └─ AEAD wrapping
DEK  256-bit, randomly generated, kept in the vault's wrapped_dek
  └─ AES-256-GCM encryption
each user secret record
```

The vault lives at:

```text
$AGIT_HOME/secret-filter/vault.json
```

The KEK never enters that file. Where it lands is the `secrets.keystore` setting
(`AGIT_SECRETS_KEYSTORE` overrides it):

- `os` (the default): `keyring`'s native store — macOS Keychain, Windows Credential Manager,
  the Secret Service on other Unix systems;
- `file`: one `<vault-id>.key` per vault under `$AGIT_HOME/keystore/`, a 0600 file in a 0700
  directory. This is for a machine with no desktop session — an SSH login, a CI runner — where
  no Secret Service answers. Its protection is the file mode, the trust an SSH private key
  rests on, which sets its boundary (§2) and makes it Unix only: on a platform without an
  owner-only file mode the credential store is the keystore. A key file the store cannot vouch
  for — a symbolic link, a file readable beyond its owner, one another user owns — is refused
  on read, never resolved; an existing key file is never overwritten; and a key is durable on
  disk, its bytes, its mode and its directory entry, before the vault that references it is
  written, so a power cut cannot leave a vault whose key never reached the disk. The directory
  is a sibling of `secret-filter/`, never inside it, so a copy of the vault directory alone
  does not carry the key.

The choice is explicit and per machine. Nothing falls through from one store to the other: a
key created under the environment of one login and looked up under another would be missing
on every other day. With the OS store selected and no usable keyring, initialization and
unlocking fail explicitly rather than degrade to a plaintext key file in the same directory;
the error names the setting, and `agit doctor` reports the keystore's state.

Every AES-GCM encryption uses a fresh 96-bit CSPRNG nonce. The AAD binds the application
domain, the vault id, the record id and the format version. An authentication failure makes
the vault unusable as a whole; a rule is never silently skipped and the scan then declared
done.

Record plaintext enters a 128/256/512/1024/2048-byte length bucket before encryption, and the
exact length stays inside the ciphertext; the outer layer exposes only the record count and
the rough length bucket. Registering the same secret twice still produces different
ciphertext.

### 3.3 Aho–Corasick, not assembled regexes

A registered entry is always a case-sensitive UTF-8 **literal**. `.`, `*`, `[` and the like
carry no regex meaning. After unlocking, one Aho–Corasick automaton is built over every
enabled record, with leftmost-longest semantics:

```text
build: O(total pattern bytes)
scan:  O(input bytes + emitted matches)
```

Overlapping hits are merged by byte range and then replaced with
`[redacted:registered-secret]`. Outward reporting carries only the opaque record id; the live
security event sent to the hub does not even carry that — only the count of new hits and the
source category — so the far side never gets an oracle that tells one device-local passphrase
from another.

### 3.4 `agitd` is the Secret Guard

The repository already has an `agitd`: resident, outbound, and constrained by a local control
socket. A second daemon would only add key-lifetime, IPC and update-consistency problems, so
there is none.

At `agitd` startup:

1. if the vault does not exist, load an empty matcher;
2. take the KEK from the keystore `secrets.keystore` selects (§3.2);
3. unwrap the DEK, then authenticate and decrypt the records one by one;
4. build an immutable matcher;
5. explicitly zero the temporary DEK and the record plaintext bytes;
6. hand the shared matcher handle to every session redactor.

A rule update triggers a reload over the local control socket. The atomic swap happens only
once the new matcher is fully built; on failure the old matcher keeps working and the failure
is reported explicitly. An existing session sees the new version immediately through the
shared handle, and holds no private copy of the rules that could drift.

## 4. CLI

Added:

```text
agit secrets add <name> [--stdin] [--allow-short]
agit secrets list [--json]
agit secrets remove <id-or-name>
agit secrets status [--json]
```

`add` uses a non-echoing password prompt on a TTY; off a TTY, `--stdin` is required
explicitly. A secret is never accepted as a command-line argument, which keeps it out of shell
history and the process list. From stdin only one terminating CR/LF is stripped; every other
space is kept verbatim.

Constraints in the first release:

- 8 UTF-8 bytes minimum by default;
- 4–7 bytes require `--allow-short`;
- under 4 bytes is rejected;
- 512 UTF-8 bytes maximum;
- a name is non-empty, at most 128 bytes, and unique on this device;
- no decrypt, show, export or arbitrary `test <candidate>` command.

A short rule that really does match natural language is not an algorithmic false positive; the
limits and the explicit confirmation bound the alert storm and the online enumeration surface.

## 5. Scan surface and handling

### 5.1 The RC live boundary

The supervisor redacts at the machine edge before handing a frame to the journal and the WSS.
Registered secrets join that same `Redactor`, so the original text never reaches the hub
first.

A delta can split one secret across several chunks. Each item keeps a streaming buffer holding
back at least `max_pattern_len - 1` unsent bytes; a prefix is released only once every pattern
that could start at that position is decided. The buffer flushes at the end of an item or
turn. Continuity spans the transport chunks and stops between two independent items or
messages.

On a registered-secret hit:

- the wire content is replaced in place;
- `secret.detected` is sent once per session and record id;
- the event carries only `count` and `source` — no secret, no name, no record id, no context;
- showing a concrete name on this device goes through the privileged local admin surface,
  queried by id, never through the hub.

### 5.2 Publishing and copies

These paths must load the same matcher:

- the repo-wide gate of `agit push` and `agit scan --secrets`;
- the pre-share check of `agit share`;
- the redacted copies of `agit export` / import;
- RC prompt, delta, approval summary and the transcript completed item.

A registered-rule hit in the publish gate uses the single rule name `registered-secret`; the
report gives the file or object location and the fixed redaction placeholder, never the
registered name.

When the vault exists but cannot be unlocked, those outbound paths fail closed: "nothing was
scanned" must not be taken for "clean". A vault that does not exist is equivalent to an empty
matcher, which preserves the behavior seen by users who have not enabled the feature.

## 6. Updates, consistency and deletion

A vault write goes through a temporary file in the same directory plus an atomic rename; the
existing file is read and authenticated in full before the update. The sequence:

```text
read and authenticate the old vault
  → modify records in memory
  → write the temporary file and flush
  → atomically replace vault.json
  → notify agitd to reload
  → complete the command once the new matcher is ready
```

With no daemon running the change still succeeds, and the next startup loads the new version.
With a daemon running but the reload failing, the command returns failure and states plainly
that the disk is updated while the running matcher is still the old one. That state must not
be reported as done.

Deleting a ciphertext record forces a matcher rebuild. The old matcher is destroyed once the
in-flight read locks release; an automaton can leave recoverable traces in the free pages of a
general-purpose allocator, so a high-security deployment rolls `agitd` after a rule change.
The first release does not claim that an allocator drop amounts to reliable zeroization.

## 7. Failure semantics

- vault missing: empty matcher, ordinary operation;
- malformed vault JSON: fail closed;
- keystore entry missing (OS keyring entry or key file): fail closed, reporting the vault as
  unrecoverable;
- the wrapped DEK fails authentication: fail closed;
- any record fails authentication: the whole matcher reload fails;
- the Aho–Corasick build fails: keep the old matcher, never publish half a rule set;
- the RC scan queue is overloaded: reuse the existing backpressure, never silently drop a
  frame;
- too many hits: merge the ranges, deduplicate alerts per record and session.

The feature in this RFC is an outbound security control, so "the vault exists but the scan did
not finish" is never clean.

## 8. Key rotation and recovery

Rotating the KEK only unwraps the DEK with the old KEK and wraps it with the new one. Rotating
the DEK requires authenticating and decrypting every record and encrypting it again. The first
release opens no CLI rotation command, but the file format reserves `key_version` and
`vault_version`.

There is no cross-device recovery by default: a database copied to another machine has no
matching keystore entry, so every value is registered again. Adding an export later takes a
separate RFC, covering user presence, a separate recovery passphrase, a slow KDF, auditing and
explicit risk confirmation; the KEK is never quietly slipped into an ordinary backup.

## 9. Acceptance criteria

Correctness:

- an ordinary low-entropy phrase hits at the start, in the middle and at the end;
- regex metacharacters hit as literals;
- Unicode, whitespace and overlapping rules behave deterministically;
- a hit spans delta chunks, and independent items never run together;
- a matcher reload takes effect after add/remove;
- push/scan and the RC redactor reach the same verdict on the same text;
- the original secret appears in no hit output, log or protocol event.

Cryptography and storage:

- encrypting the same record twice yields different ciphertext;
- modifying the nonce, the ciphertext or the AAD fails authentication;
- the database holds no plaintext, no exact length and neither KEK nor DEK;
- a missing keystore entry and a corrupt record both fail closed;
- a failed atomic write leaves the old vault intact.

Performance and resources:

- the matcher is built once, and a scan does not re-traverse the input once per rule;
- large texts, shared prefixes and high hit density all have bounded behavior;
- streaming state is released at the end of an item or session and on timeout;
- sustained throughput reaches at least twice the target production peak, with the concrete
  figure fixed by a benchmark over a real corpus.
