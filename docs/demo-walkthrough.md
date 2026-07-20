# agit — end-to-end demo walkthrough

A step-by-step script to showcase **every** agit feature to a mentor, from an empty
directory to a full team + hub workflow. Follow it top to bottom; each act builds on
the last. Commands are copy-pasteable. "**They see:**" notes call out what to point at.

- **Hub:** https://agit.anggita.org  (open self-service registration)
- **Client:** the `agit` binary (`agit --help` for the full verb list)
- **Two "people":** to demo collaboration on one laptop, run the second person's commands
  with a separate home: `HOME=/tmp/bob AGIT_HOME=/tmp/bob/.agit agit …`

> Concepts in one breath: an **Agent Store** is a git repo of an AI agent's *memory*
> (its session transcripts + harness). `agit <git>` acts on your **code** repo; `agit a <git>`
> acts on the **agent store**. The **hub** hosts agent stores like GitHub hosts code.

---

## Act 0 — Setup (once)

```bash
# 1. Confirm the client is installed
agit --help | head -3

# 2. Make an account on the hub (or use the web UI at https://agit.anggita.org/register)
#    Then create a WRITE TOKEN in the web UI:  Login → /tokens → New token.
#    Export it so pushes authenticate:
export AGIT_HUB_URL=https://agit.anggita.org
export AGIT_HUB_TOKEN=<the token you just made>
```

**They see:** a real, TLS-served hub with a clean web UI (login, register, orgs, tokens).

---

## Act 1 — From zero: attach an agent to a repo

```bash
# 1. Start from ANY code repo — a fresh one or an existing project
mkdir demo && cd demo && git init -q && echo "# Demo" > README.md && git add -A && git commit -qm "init"

# 2. Attach an agent to this repo (mints or selects the agent, installs the secret hooks)
agit init

# 3. Do some AI work here with claude-code or codex (or resume an existing session).
#    One command captures AND commits that work into the Agent Store (gated: a suspected
#    secret is mirrored to disk but held out of history until you resolve it):
agit a snap
```

**They see:** `agit a status` — this repo, its agent, the captured sessions, last activity.
(`agit a snap` already committed them — there is no separate commit step.)

```bash
agit a status
agit a log     # the SESSION timeline (prompts + edits over time), not raw git
```

---

## Act 2 — Hands-off capture + cross-runtime

```bash
# Auto-capture: watch claude-code AND codex, snap + convert both ways, forever
agit watch --daemon
agit watch --status      # confirm it's running   (agit watch --stop to end)

# Cross-runtime: take a claude-code session and continue it in codex (or vice-versa)
agit convert <session> --to codex --write
agit resume <session> --as codex
```

**They see:** you never manually save — sessions from *both* runtimes flow into the store,
and a session authored in one tool resumes in the other. This is agit's headline trick.

---

## Act 3 — Provenance: who really wrote this

```bash
# Every captured session is signed by this machine's key
agit provenance key                 # this machine's public signing key
agit provenance verify <session>    # checks the signature against its recorded key

# Enroll your identity on the hub so pushes are attributed to your ACCOUNT (not just a key)
agit identity enroll                # publishes your signing + encryption public keys + email
agit identity show                  # your fingerprints + enrollment status
```

Then verify your email (Account page → the operator-forwarded link, or `agit-hub user verify-link <you>`
server-side). Once verified, the hub shows a green **"verified · you"** badge on your sessions;
a session signed by a *different* key claiming your email shows a red **"key mismatch."**

**They see:** the trust chain — sign → enroll → verify email → **verified badge** in the web UI.

---

## Act 4 — Publish to the hub

```bash
# 1. Create the store on the hub:  web UI → "New agent" (New)  →  e.g.  <you>/demo
#    (Private by default.)
# 2. Push your agent's memory to it (token from Act 0):
agit a push $AGIT_HUB_URL/<you>/demo.git
```

Open **https://agit.anggita.org/agent/<you>/demo** in the browser.

**They see (web UI tour):**
- **Sessions** list + a single session view (the full transcript, rendered)
- **Diff** between two points in the agent's memory
- The **provenance badge** (Act 3) next to each session
- **Merge requests** — open one, review, comment, close
- **Account** → set up **2FA** (scan the QR, enter the 6-digit code, save backup codes)
- **Orgs**, **Audit log**, **Tokens**

---

## Act 5 — Collaborate: clone, pull, merge

```bash
# A teammate (second identity) adopts the agent BY IDENTITY and gets its full memory
HOME=/tmp/bob AGIT_HOME=/tmp/bob/.agit \
  agit a clone $AGIT_HUB_URL/<you>/demo.git

# They do their own AI work, snap, and push back
HOME=/tmp/bob AGIT_HOME=/tmp/bob/.agit agit a push

# You pull their memory back
agit a pull

# Merge two agents' memories BY DIALOGUE (not a code branch merge)
agit a merge <other-agent>          # same agent → histories merge; different agent → dialogue only
```

**They see:** two people building one shared agent memory, merged intelligently — the whole
point of agit ("collaborate on Agent Context").

---

## Act 6 — Security: the secret gate

```bash
# agit scans session dumps for secrets on every commit/push — unbypassable on agit's own paths
echo 'AWS_SECRET_ACCESS_KEY=AKIA...' >> notes.txt   # (pretend a secret leaked into a session)
agit a snap
agit a push          # ← the gate REFUSES, naming the finding (override is explicit + visible)
```

**They see:** a real secret blocked before it ever reaches the hub, with a clear message
(and `AGIT_ALLOW_SECRETS=1` as the *visible* override — never silent).

---

## Act 7 — Encryption: readable-by, decided by cryptography

```bash
# Readable by SPECIFIC people (wraps the content key to their enrolled keys)
agit a encrypt --readers alice,bob
agit a readers ls
agit a readers add carol            # O(1), no re-encryption
agit a readers rm carol             # EAGER rotation — carol can't read NEW content

# Readable by ANYONE with the repo
agit a encrypt --public

# Readable by your TEAM (org members), the zero-config default under an org
agit hub team rekey <org>           # once: establish the team key
agit a encrypt                      # (org-owned session) → team-readable, NOT public

# On another machine, after cloning:
agit crypt unlock                   # unwraps the content key with YOUR key; fails closed if you can't
```

**They see:** the hub stores only ciphertext for a private encrypted store — it literally
**cannot render or scan it** (agit prints that warning). Confidentiality enforced by keys,
not by trusting the server. Provenance/ACL still work; encryption is the second axis.

---

## Act 8 — Teams & orgs

```bash
# Org lifecycle (web UI → Orgs, or CLI)
agit-hub org invite <org> <user> --role member     # invite → the user Accepts in their inbox
agit-hub org transfer <org> <new_owner>            # hand over ownership
# Create a store UNDER the org (web "New agent" with the org selected; org admins today)

# Team key lifecycle
agit hub team sync <org>            # a new member joined → seal the team key to them (O(1))
agit hub team rekey <org>           # someone left → rotate; they lose access to NEW content
agit hub doctor --org <org> --check # reconcile: who's authorized-but-can't-decrypt, or vice-versa
```

**They see:** invitations with consent, roles, and the team-encryption lifecycle
(join/leave) all working.

---

## Act 9 — Production-grade lifecycle (advanced)

```bash
# Multi-remote: push to several hubs at once with asymmetric permissions
agit a push                         # fans out to every bound remote

# Opt-in escrow (org admin) — trades some hub-trust for real revocation + recovery
agit hub org escrow <org> --mode hub-assist
agit hub org recovery set <org> --key <hex>      # offline recovery recipient

# Scrub pre-encryption plaintext out of git history (guard-railed, rewrites SHAs)
agit a purge-history

# Snapshot / restore the paired state of code + agent together
agit workspace log
agit workspace restore <N>
agit graph                          # the Workspace-State timeline + relation edges
```

**They see:** this isn't a toy — key rotation, escrow, history purge, paired code↔agent
snapshots, multi-remote fan-out.

---

## Act 10 — Hub as a product (web UI)

Walk the mentor through, logged in at https://agit.anggita.org:
- **Home / agent pages** — browse hosted agents, sessions, diffs, provenance badges
- **Merge requests** — review + comment + close
- **Account** — 2FA (QR + backup codes), email verification, signing keys
- **Orgs** — membership, invitations, escrow/recovery settings
- **Admin** (as an admin) — user recovery tools
- **Audit log** — every security-relevant action recorded
- Backend: **Postgres + Garage (S3)**, TLS via Let's Encrypt, `/metrics` for Prometheus

---

## One-line pitch to close

> "agit is git for an AI agent's memory: capture every claude-code/codex session, sign and
> attribute it, collaborate on it like code, encrypt it end-to-end with per-session
> readable-by sets, and host it on a hub that works like GitHub — running live at
> agit.anggita.org."

---

### Notes / caveats to keep the demo smooth
- Create the hub store **before** `agit a push` (create reserves the name; push populates it). At
  creation you can check **"initialize with an empty agent"** so a teammate can clone it before your
  first push, and a clone of a not-yet-initialized store now explains what to do instead of erroring.
- Enrolling from a second machine **adds** that machine's device key to your account (the multi-key
  "SSH-keys" model), so provenance verifies against any machine you enrolled; a second machine no longer
  replaces your key.
- Email verification has no SMTP wired yet: grab the verify link from the hub logs /
  `agit-hub user verify-link <you>` when demoing the verified badge.
