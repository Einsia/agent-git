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

The default interactive menu requires a terminal. Without one, it exits with
code `8` and prints the PAT command. The explicit `--device` option bypasses
that menu and can print its approval instructions without a terminal.

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
as evidence that the token is valid. Missing credentials return code `5`.

`agit logout` attempts to revoke the current Hub session before deleting the
local credentials. `agit logout --all` does this for every saved Hub. If the
server is unreachable, the command warns and still clears local credentials;
it cannot guarantee that the server session was revoked. An unreadable legacy
credential or one without a recoverable Hub address has the same limitation.
Captured sessions and local Agent repositories remain available after logout.

## Code locations

`src/commands/{login,logout,whoami}.rs`, `src/infra/{config,credentials}.rs`, and
`src/hub/client.rs` implement the CLI behavior. The backend routes live in
`src/router.rs` and `src/features/auth/` in the backend repository.
