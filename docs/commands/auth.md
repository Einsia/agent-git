# `agit login` / `agit logout` / `agit whoami`

The CLI signs in through browser authorization, device code, or a personal
access token (PAT) read from stdin. Account authentication happens on the Hub
website; the CLI does not ask for a username or password.

## Choose a sign-in flow

| Command | Behavior |
| --- | --- |
| `agit login` | Open an interactive menu. Press Enter for browser authorization, or select device code. |
| `agit login --device` | Skip the menu and print a verification URL and code to approve in a browser on any device. |
| `agit login --with-token < token.txt` | Read a PAT from stdin and exchange it for a Hub session. |

Browser authorization opens the approval URL when possible and also prints it.
Device code works when the CLI machine cannot open a browser, including SSH
sessions and containers. Both flows wait for approval and can expire; device
code still requires a person to approve it on the website. Use PAT input for
unattended CI and agent jobs.

Without a terminal, or with `--json`, `agit login` returns at once with exit
code `8`, a login link for the human, and the `--complete` command that finishes
it. The explicit `--device` option bypasses the interactive menu and can print
its approval instructions without a terminal.

## Finish a sign-in from another process

Every browser and device-code request is recorded under `$AGIT_HOME/credentials/`
as soon as the Hub creates it, one per Hub, with the same private file mode and
locking as the credentials. The record holds the polling value: a device code is
never printed, and a browser state appears only in the `--complete` command that
the non-interactive handoff prints. `agit login --complete` without a value
claims the recorded request of the selected Hub from any later process, so an
agent runtime that stops the waiting `agit login` or `agit login --device` does
not lose the approval. With a value it finishes that browser request, as the
printed command does. Either form, run again after the request was claimed on
this machine, reports the sign-in that already happened. Without a value and
with no request for the selected Hub, it names another Hub whose request waits.

`--complete` checks at once and then keeps polling at the Hub's interval for up
to 90 seconds, or `--wait <seconds>` (`0` checks once; at most ten minutes),
never past the request's expiry. A poll that fails in transit or with a server
error does not end the wait. If the approval is still missing it exits `8` and
keeps the request, and running `--complete` again continues; for a device-code
request it repeats the verification link and code. If the request expired or was
already used, the record is removed and the command says to run `agit login`
again. Any other refusal from the Hub also removes the record and reports the
Hub's error; a refusal from a proxy or firewall in front of the Hub keeps it.

A new `agit login` first claims a recorded request the human already approved,
and otherwise replaces the recorded request with its own. Processes finishing,
replacing or expiring the same request take turns: one started while another
polls or saves the approved session waits and then reports that sign-in, and a
new request replaces the recorded one only between two polls of it, so it never
discards a session the human approved. Signing out forgets the request,
including `agit logout --all` when no credentials are saved. A claim whose poll
is in flight when `agit logout` forgets the request saves nothing: the session it
receives is signed out again, `agit login` exits `8` saying the sign-in was
cancelled, and any other command continues as not signed in.

`agit whoami --check`, `agit commit` and commands that stop with "not logged in"
first make one short, non-waiting attempt to claim an approved recorded request,
then continue signed in; while the request still waits they name
`agit login --complete`. `agit search --local`, which makes no Hub request, only
names it.

Use `--hub <url>` to choose the Hub for a login. Otherwise the order is
`AGIT_HUB_URL`, `config hub.url`, then the built-in public Hub. A successful
login with `--hub` also saves that address as the configured default.

```bash
agit login --hub https://dev.agent-git.com
agit login --hub https://dev.agent-git.com --device
agit login --hub https://dev.agent-git.com --with-token < token.txt
```

## Authorization and credential storage

Browser authorization creates a request with `POST /api/auth/cli/session` and
polls `POST /api/auth/cli/poll`. Device code creates a request with
`POST /api/auth/device/code` and polls `POST /api/auth/device/token`. The CLI
saves credentials only after the Hub returns a session.

Credentials live under `$AGIT_HOME/credentials/`, with `~/.agit` as the default
home. The CLI chooses a bounded filename from the validated Hub authority. Each file contains the access and refresh tokens,
their expiry timestamps, the Hub address, username, and optional email. The
authority includes an explicitly supplied port; omitting the port is a different
identity from spelling it explicitly. Host and HTTP scheme lettercase are
normalized; changing the URL scheme or path does not create another identity. Unix credential files have mode `0600`; Windows
writes use the current user's private access control list.

Requests carry the access token. On an authentication failure the Hub client
can refresh and retry. Refresh tokens rotate, so processes sharing a credential
file can adopt a newer pair saved by another process for the same account.
Credentials for a different account are not substituted during that recovery,
and a completed login or logout cannot be overwritten by an in-flight refresh.

Existing host-key files are reusable only when their recorded Hub agrees with
the requested authority and the filename matches that record. Reads preserve
those files. Missing Hub metadata, conflicting records, or an invalid current
credential file require signing in again; a rejected current file does not fall
back to an older token. Scripts should use `login --with-token` rather than
constructing credential filenames or JSON.

Recording a session uses the saved username and email for the Agent repo's Git
author identity; a missing email falls back to `<username>@agit.local`. Login
does not create an Agent repo or bind a workspace. Public read-only cloning
does not require login; cloning into your namespace with `--mine` and publishing
do. Offline adoption without recording a version uses `agit import --link-only`.

## Session authorship and integrity

The current protocol has no AgentGit signing-key store, public-key enrollment,
or signature verification badges. Hub sessions and repository grants authorize
access. Git author fields record attribution, and Git object hashes identify
content; neither is proof of a signing identity.

## Inspect or revoke a session

`agit whoami` displays the selected Hub, saved identity, and credential expiry
without checking the server. `agit whoami --check` verifies authentication
through the Hub's account endpoint; it does not treat a public health response
as evidence that the token is valid. Missing credentials return code `5`, after
`--check` has tried to claim a recorded sign-in the human approved.

`agit logout` attempts to revoke the current Hub session before deleting the
local credentials. `agit logout --all` does this for every saved Hub. Both
forget the waiting sign-in requests before reading the credentials, and revoke
credentials a concurrent sign-in saved after that read when they remove them, so
every session they delete is one they asked the Hub to revoke. If the server is
unreachable, the command warns and still clears local credentials; it cannot
guarantee that the server session was revoked. An unreadable legacy
credential or one without a recoverable Hub address has the same limitation.
Captured sessions and local Agent repositories remain available after logout.

## Code locations

`src/commands/{login,logout,whoami}.rs`, `src/infra/{config,credentials}.rs`, and
`src/hub/client.rs` implement the CLI behavior. The backend routes live in
`src/router.rs` and `src/features/auth/` in the backend repository.
