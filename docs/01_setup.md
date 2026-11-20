# Build and run

The backend is a pure JSON API with no web interface — `GET /` is a 404. Check
its state with curl or `agit doctor`.

## Install the CLI

Several paths, the same binary either way:

| Who                      | How                                                                  | Notes                                               |
| ------------------------ | -------------------------------------------------------------------- | --------------------------------------------------- |
| Users (one-shot)         | `npx -y create-agit@latest`                                          | one-shot install to `~/.local/bin/agit`, then setup |
| Users (global)           | `npm install -g @einsia/agent-git --allow-scripts=@einsia/agent-git` | prebuilt binary; its install script runs setup      |
| People changing the code | `./setup.sh`                                                         | builds from source, installs to `~/.local/bin/agit` |

The global package wires skills, hooks and MCP from its install script. npm 12 and later
skip an install script that has not been allowed, which is what `--allow-scripts` above is for;
an organization policy (`strict-allow-scripts`, `ignore-scripts`) can block or skip it too. Under
pnpm (`pnpm add -g @einsia/agent-git`) the install script configures nothing. If the install
script did not run, run `agit setup` once yourself. It sets up the runtimes installed at the time,
so run it again after installing another. What the npm package itself does is in
[`../npm/README.md`](../npm/README.md).

### Hook installation and controls

Global npm installation and create-agit normally invoke `agit setup`; local npm
dependencies/prefix installs and npm exec do not use the global setup path.
`AGIT_SKIP_SETUP=1` skips setup and reminders, not invocation of hooks already
installed or other installer work. An ordinary setup reminder previews changes
and asks before installing. Upgrade refreshes installed Skills and can configure
new runtimes after an earlier setup, respecting a declined setup and Skill opt-outs.

Setup prints the target before writing and distinguishes `updated` from
`already configured`. Claude uses the selected profile's `settings.json`
(`CLAUDE_CONFIG_DIR`, normally `~/.claude`); Codex uses `$CODEX_HOME/hooks.json`
(normally `~/.codex/hooks.json`). npm may hide setup's output. Open the hook
entries in those files to inspect them; avoid sharing the whole configuration.

| Configured action | Purpose |
| --- | --- |
| SessionStart: `agit hooks ingest` | Connect session state and show capture/upload notices |
| Stop: `agit hooks settle` | Save eligible conversation turns; upload follows the folder/repository settings |
| Claude on Unix, PermissionRequest: `agit hooks native-permission` | Relay remote-control permission requests while the native prompt stays available |
| Claude on Unix, SessionStart: `agit hooks relay` | Receive remote-control messages asynchronously |

Codex commands include `--runtime codex`. It requires approval of the exact
command and enabled hooks; use its `/hooks` review and controls. Claude's
`agit doctor` row checks configuration presence only, not effective execution
or save/upload results. Other runtimes use their own events or native plugins.
An existing Claude RC integration may also receive the SessionStart relay during
ordinary startup; that migration respects `disableAllHooks` and does not add Stop.

| To do this | Use | Boundary |
| --- | --- | --- |
| Pause a folder's automatic capture | `agit off [DIR]` | Hooks/history remain; explicitly linked sessions can still save locally |
| Stop automatic turn saving | `agit config --global commit.auto false` | Hooks and SessionStart remain; queued work is not undone. Restore with `true`; explicit commits remain available |
| Stop a hook's invocation | Disable/remove only that AgentGit hook entry in the runtime configuration | Keep other hooks and settings. A later full setup can add the entry again |

There is no `agit setup --remove-hooks` option; removing the Skill does not remove
Claude/Codex hooks. Hook installation alone does not enroll an arbitrary folder
for upload, and Stop saves conversation history rather than a code commit.
Synchronous Stop paths can wait on local saving or upload. The separate Claude
reply-wait timeout and relay's async setting do not bound the whole Stop hook.

Do not install `@einsia/agentgit` (no hyphen) — that is a different CLI from
before the rewrite, and its protocol does not match this branch.

Prebuilt distribution binaries include Git and Git LFS. The CLI extracts its private
runtime under `$AGIT_HOME/git-runtime` (by default `~/.agit/git-runtime`) on first
use, without downloading additional files. Your shell PATH and global Git profile
are unchanged. User and repository Git configuration still apply; AgentGit's own
repository settings continue to control its internal commits and LFS filters.

Source builds use system Git by default and require **Git >= 2.28**; lineage
inspection needs Git >= 2.36 and LFS operations need **Git LFS >= 3.7.1**.
`AGIT_USE_SYSTEM_GIT=1` also selects system Git in a distribution build, including
when a custom credential helper or extension requires your local Git installation.
Run `agit doctor` to see the selected Git and LFS versions. See
[the bundled runtime guide](bundled-git.md) for packaging and cache details.

### The Linux artifact is musl-static

On Linux the npm path downloads `*-unknown-linux-musl`, not gnu. The reason is a
real incident: a gnu artifact turns the **build machine's** glibc version into a
runtime floor, and the user gets `libc.so.6: version 'GLIBC_2.38' not found`
straight after installing. GitHub's ubuntu-latest is now 24.04 (glibc 2.39), and
the gnu artifact built there starts on none of Debian 12, Ubuntu 22.04,
Debian 11, Amazon Linux 2.

The musl artifact is static-pie with no glibc floor: every environment above
plus Alpine runs it. Before building the Release, the release pipeline puts
every Linux artifact into an alpine / amazonlinux / debian / ubuntu container
matrix and actually runs it there (the smoke-test step of `release.yml`).

The embedded Linux Git runtime carries its own musl loader and shared libraries.
It invokes that loader explicitly and uses the host's `/bin/sh`, so Git does not
introduce a host glibc requirement into the static CLI distribution.

`setup.sh` checks the toolchain before building; when something is missing it
tells you which command to type instead of an error from deep inside cargo. For
manual control, read on.

## Build

Needs **`cargo >= 1.88`** and a C compiler.

The two floors come from different places and are not the same number: edition
2024 itself only needs 1.85, but in the committed `Cargo.lock`, `darling 0.23`
and `instability 0.3.12` (a ratatui dependency) declare `rust-version = 1.88`,
so the real floor is 1.88. (1.78 is wrong — edition 2024 does not compile on it
at all.) The C compiler is what `rusqlite`'s `bundled` feature needs: it
compiles sqlite's C source into the binary, so the target machine does not need
sqlite installed.

rustup installs into `~/.cargo/bin`; when that is not on PATH, `source
~/.cargo/env` first.

```bash
(pushd AgentGit-backend && cargo build --release && popd)   # agentgit-backend + agentgit-admin
(pushd agent-git        && cargo build --release && popd)   # agit
```

Run the tests: `cargo test` (69 in the backend), `cargo test --lib` (166 in the
CLI). Without `--lib` the CLI suite times out on this NFS machine.

## Run the backend

Zero configuration:

```bash
./AgentGit-backend/target/release/agentgit-backend
# INFO AgentGit backend started addr=127.0.0.1:8177 root=~/.agentgit-backend
```

The sqlite database and the bare git repos both live in `~/.agentgit-backend`
(0700), and the tables are created at startup. `AGIT_BACKEND_ROOT` /
`AGIT_BACKEND_PORT` change the location and the port.

```bash
curl -s http://127.0.0.1:8177/api/health
# {"status":"ok","version":"0.1.0"}
```

Binding a non-loopback address is refused unless `--insecure` is passed
explicitly — this backend holds every session transcript the team has, and
without TLS both tokens and transcripts cross the network in the clear. In
production, put a reverse proxy in front to terminate TLS.

## Create an account

Self-service registration is open by default: `/signup` in the web interface
creates an account, or call the API directly:

```bash
curl -s -X POST http://127.0.0.1:8177/api/auth/register \
  -H 'content-type: application/json' \
  -d '{"username":"alice","password":"your-long-password"}'
# returns a token pair; registering signs you in
```

On an instance with self-service registration off
(`AGIT_BACKEND_OPEN_REGISTRATION=false`), an administrator creates the accounts.
The password is read from the terminal, never from argv:

```bash
./AgentGit-backend/target/release/agentgit-admin user-add alice
# set a password:
# repeat it:
# created account alice

./AgentGit-backend/target/release/agentgit-admin user-list
```

The only password rule is length ≥10.

With no tty (`docker exec -i`, a pipe) it prints `stty: Inappropriate ioctl for
device`. That is noise; the account is still created correctly. In a script,
pipe the password in twice:

```bash
printf 'your-long-password\nyour-long-password\n' \
  | ./target/release/agentgit-admin user-add alice 2>/dev/null
```

## login / logout

The CLI connects to the public hub `https://agent-git.com` by default, so `agit
login` works as soon as it is installed.

This document covers a self-hosted instance, so point every example below at the
one you started yourself first:

```bash
export AGIT_HUB_URL=http://127.0.0.1:8177
```

For an acceptance binary that defaults to staging without depending on a runtime
environment variable:

```bash
AGIT_DEFAULT_HUB_URL=https://staging.agent-git.com cargo build --release --locked
```

Credentials are stored per address, so the public hub and a self-hosted instance
each stay signed in; switching the environment variable switches identity.

```bash
agit login              # authorize through the browser or choose device code

agit doctor --check-backend
#   [✓] sign-in          alice @ http://127.0.0.1:8177
#   [✓] backend          ok @ http://127.0.0.1:8177 (version 0.1.0)

agit logout             # revokes the server session + deletes local credentials, not the store
```

Signing in returns a token pair, stored under `$AGIT_HOME/credentials/` (default
`~/.agit/credentials/`). The CLI chooses the filename and binds its contents to
the selected Hub authority. Unix files are private to their owner; Windows
files use the current user's private access control list. The credential scope,
refresh behavior, and compatibility rules are in
[`commands/auth.md`](commands/auth.md).

CI uses a PAT supplied through stdin. Save the PAT as a secret file in the CI
runner, then let the CLI validate and store the session:

```bash
export AGIT_HOME=/tmp/agit-ci AGIT_HUB_URL=http://127.0.0.1:8177
agit login --with-token < token.txt
agit whoami --check
```

Do not construct credential filenames or hand-write token JSON. Records without
a valid Hub address cannot establish which destination may receive their tokens.

## Troubleshooting

For owner-controlled-state or ancestor-permission failures after using a shared
shell umask, see [bounded local permission recovery](state-permissions.md).

| Symptom | Cause |
|---|---|
| `cannot connect to the backend ...` | the backend is not running, or `AGIT_HUB_URL` has the wrong port |
| `the backend refused authentication (401)` | the refresh token expired too (thirty days); log in again |
| `wrong account or password` | wrong password, or no such account — the wording deliberately does not distinguish, to block account enumeration |
| `GET /` returns 404 | expected; there is no web interface |
| `stty: Inappropriate ioctl` | noise; the account is still created |

```bash
agentgit-admin doctor        # account / agent / session counts + repo disk use
agentgit-admin audit -n 20   # audit records
```

An unauthenticated `/api/agents` returns 200 and an empty list rather than 401:
an anonymous caller can see public agents. A private agent the caller cannot
reach returns 404 rather than 403; otherwise agent names could be enumerated.
