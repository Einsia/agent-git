# `agit login` / `agit logout`

One path only: a username and password buy a pair of tokens. Accounts
self-register at `/signup` in the web interface (email optional); on an instance
with self-registration turned off an administrator creates them with
`agentgit-admin user-add [--email <email>]`.

Sign in with **either the username or the email** — an input containing `@` is
looked up as an email, anything else as a username.

## Mechanism

```
agit login                    username (or email) and password (tty required)
  → POST /api/auth/login      → {username, email, access_token, refresh_token, both expiries}
  → stores $AGIT_HOME/credentials.json (0600)

later requests                carry Authorization: Bearer <access_token>
  → on a 401 the client refreshes once and retries
     POST /api/auth/refresh   {refresh_token} → a new pair

agit logout
  → POST /api/auth/logout     the server deletes the session row
  → deletes the local credentials
```

The two tokens divide the work: **access lasts one hour** and rides on every
request; **refresh lasts 30 days** and only buys a new access. A leaked access
token is exposed for only one hour, while one sign-in lasts a user a month.

## Sign-in also stores the username and email

The credentials carry two more things: `username` and `email`. `agit commit`
uses them to set `user.name` / `user.email` on the Agent repo's git — who
recorded a commit is recorded through exactly this, the same as GitHub; there is
no separate signing mechanism. A missing email falls back to
`<username>@agit.local`.

## Three properties worth knowing

**A refresh token is single-use.** A refresh deletes the whole row server-side
and inserts a new one, so a second use of an old refresh fails. When one is
stolen, the real user's next refresh fails — a detectable signal. When several
clients / processes hold the same credentials in memory at once, whichever
refreshes first persists the new pair, and a client that arrives later reads the
disk on its 401: a newer pair for the same account is adopted directly, and a
client whose own refresh fails waits, bounded, for a sibling process to persist
and then adopts. Credentials for another account are never taken over.

**Only digests are stored.** The server's `sessions` table stores
`sha256(plaintext)` for both tokens. A leaked database yields no directly
reusable token. The plaintext appears once, at issue time.

**A failed sign-in does not distinguish the reason.** A wrong password and a
nonexistent account both return "wrong username or password", and for a
nonexistent account the server still runs one password hash verification — so
the response time does not leak whether the account exists either.

## Details

* Credentials are stored per hub address; switching `AGIT_HUB_URL` switches
  identity with no new sign-in
* `login` requires a tty. A non-interactive environment is refused explicitly
  instead of hanging or guessing
* A malformed expiry timestamp: the client treats it as not expired (the
  server's 401 arbitrates); the server treats it as expired (fail closed). The
  asymmetry is deliberate
* `logout` notifies the server before deleting locally. An unreachable server
  only warns and does not block — the local credentials must be cleared
* Signing in is a hard precondition for `push` and `clone` — both write
  something to the server (`clone` creates an agent under your name)
* Recording a version (`commit`, and `import`, which records a first version by
  default) also needs a sign-in, for a different reason: it is a purely local
  action and all it wants is the **username and email** in the credentials —
  they go into git's `user.name` / `user.email`, and the Agent repo path is
  `agents/<owner>/<name>/`; neither can be filled in afterwards. To adopt
  offline without recording a version, use `agit import --link-only`; `log` /
  `show` / `status` / `doctor` never need it

## Code locations

`src/commands/{login,logout}.rs` · `src/infra/credentials.rs` ·
`src/hub/client.rs::with_retry` (refresh-on-401) ·
backend `features/auth/{account,session,routes}.rs`
