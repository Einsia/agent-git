# Build and run

The backend is a pure JSON API with no web interface — `GET /` is a 404. Check
its state with curl or `agit doctor`.

## Install the CLI

Several paths, the same binary either way:

| Who                      | How                                                          | Notes                                               |
| ------------------------ | ------------------------------------------------------------ | --------------------------------------------------- |
| Users (one-shot)         | `npx -y create-agit`                                         | one-shot install to `~/.local/bin/agit`, then setup |
| Users (global)           | `npm install -g @einsia/agent-git`                           | prebuilt binary, no Rust toolchain needed           |
| People changing the code | `./setup.sh`                                                 | builds from source, installs to `~/.local/bin/agit` |

pnpm (since v10) does not run a dependency's install
scripts by default, so the automatic `agit setup` is skipped — run it once by
hand after installing. What the npm package itself does is in
[`../npm/README.md`](../npm/README.md).

Do not install `@einsia/agentgit` (no hyphen) — that is a different CLI from
before the rewrite, and its protocol does not match this branch.

Whichever path you take, you also need **git >= 2.28**: store initialization uses `git init
--initial-branch=main`, a flag that arrives in 2.28. Ubuntu 20.04 ships 2.25, so first
`sudo add-apt-repository ppa:git-core/ppa && sudo apt update && sudo apt install git`.

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
agit login              # needs a tty, prompts for the password
# username: alice
# password: [not echoed]
# ✓ signed in as alice

agit doctor --check-backend
#   [✓] sign-in          alice @ http://127.0.0.1:8177
#   [✓] backend          ok @ http://127.0.0.1:8177 (version 0.1.0)

agit logout             # revokes the server session + deletes local credentials, not the store
```

Signing in returns a token pair, stored one file per hub in
`~/.agit/credentials/<hub-host-key>.json` (0600, for example
`127.0.0.1_8177.json`; see `infra::credentials`): access lasts an hour, refresh
lasts thirty days. Once access expires the client swaps in a new one and
retries, so one sign-in lasts a month. The mechanism is in
[`commands/auth.md`](commands/auth.md).

> The older single-file `~/.agit/credentials.json` with a `hubs` map is no
> longer read. A script that hand-writes credentials in that format gets a
> misleading `not signed in`.

CI has no tty: call the API for the tokens and write the credential file
yourself:

```bash
export AGIT_HOME=/tmp/agit-ci AGIT_HUB_URL=http://127.0.0.1:8177
curl -s -X POST $AGIT_HUB_URL/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"alice","password":"your-password"}' \
  | python3 -c "
import json,sys,os,pathlib
d=json.load(sys.stdin)
home=pathlib.Path(os.environ['AGIT_HOME'])
cred_dir=home/'credentials'; cred_dir.mkdir(parents=True, exist_ok=True)
# the host key algorithm is in infra::config::hub_host_key: drop the scheme,
# **drop the path**, replace ':' with '_'. Without the drop-the-path step, a hub
# configured with a subpath (https://h/agit/) computes a filename containing a
# slash, and the CLI never reads the credentials written there.
host=os.environ['AGIT_HUB_URL'].split('://',1)[-1].split('/',1)[0].replace(':','_')
keys=('username','access_token','access_expires_at','refresh_token','refresh_expires_at')
p=cred_dir/f'{host}.json'
p.write_text(json.dumps({k:d[k] for k in keys}))
os.chmod(p, 0o600)
"
```

## Troubleshooting

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
