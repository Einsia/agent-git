# agit

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)

Lossless version control for every agent session: publishable, resumable.
Works with Claude Code, Codex, OpenCode, and Cursor.

On disk, sessions are just JSONL files that get overwritten, compacted, and
cleaned up at any moment. `agit` puts snapshots and versions on top of them,
so "that conversation last Wednesday that finally cracked the bug" becomes
something you can find, continue, and hand to a teammate.

```
agit import <session> -n <agent>   adopt an existing claude / codex / opencode / cursor session and record its first version
agit commit <agent>                record another version (snapshot of the session's full current content)
agit push <agent>                  publish to the hub, get a link
agit clone <owner>/<agent>         fetch (with git history) and pick up right away; --mine creates a copy under your name
agit upgrade                       upgrade the CLI itself to the latest release the hub announces
```

On user-facing startup, `agit` checks for a newer release at most once a day and prints a
reminder to stderr; it never upgrades automatically. JSON/quiet/CI and internal hook/MCP paths
skip this reminder.

Adopting and recording the first version are one command — the in-between
state ("linked, but unversioned") means nothing to anyone. To mark a session
without versioning it (e.g. offline), pass `--link-only`.

`agit clone` is **read-only by default**: nothing is created in your name,
`origin` points at the source, the session is installed into your runtime and
you can `agit commit` locally as usual. When you decide to take over, use
`--mine` — that creates your copy on the hub, repoints `origin` at it, and
remembers the source as `upstream`. Running `agit push` from a read-only
checkout offers exactly that.

The full command list is in `agit --help`.

## Install

### Users: npm

```sh
npx -y create-agit                 # one-shot: installs agit + wires skills/hooks/MCP
# or as a global package:
npm install -g @einsia/agent-git   # pnpm add -g @einsia/agent-git works too
agit --version
```

Both routes install a prebuilt binary — no Rust toolchain required. npm
picks the platform sub-package matching your `os`/`cpu`; on platforms with no
prebuilt binary (e.g. Windows) running `agit` prints the source-build recipe.
Details and environment variables: [`npm/README.md`](npm/README.md).

pnpm (v10+) does not run dependency install scripts by default, so the
automatic `agit setup` is skipped there — run `agit setup` once yourself
after installing.

The Linux artifact is **musl static-linked**, so no minimum glibc — the
release pipeline runs every Linux binary inside an Alpine / Amazon Linux /
Debian / Ubuntu container matrix before publishing.

You also need **git >= 2.28** (repo init uses `git init --initial-branch`).
Ubuntu 20.04 ships 2.25 — the installer prints a warning.

> Do not install `@einsia/agentgit` (no hyphen) — that is the pre-rewrite
> CLI; its protocol does not match this branch and it will not work.

### Contributors / eager users: from source

```sh
git clone https://github.com/Einsia/agent-git
cd agent-git
./setup.sh
```

`setup.sh` checks the toolchain (rustc, a C compiler), builds, and installs
the binary to `~/.local/bin/agit`. Common flags:

```sh
./setup.sh --debug      # no LTO, much faster while hacking
./setup.sh --test       # run cargo test --lib before installing
./setup.sh --help
```

To build a binary whose built-in hub is the staging deployment, set the build-time default
explicitly. Runtime `AGIT_HUB_URL` and `agit config set hub.url ...` still take precedence:

```sh
AGIT_DEFAULT_HUB_URL=https://staging.agent-git.com cargo build --release --locked
```

Internal GitLab pipelines package commit-addressed `dev` and `staging` builds with this setting
embedded. Their `agit --version` output includes the channel and source commit, and `agit upgrade`
is disabled for them so an acceptance-test binary cannot silently turn into the public release.
Only an existing GitHub `agit-v*` tag produces the `prod` channel. The tag push runs the Release
workflow automatically; operators may also select the same reviewed tag through the workflow's
manual production input to retry a failed release without creating a different artifact identity.

Requires **rustc >= 1.88** and a C compiler (`rusqlite` uses the `bundled`
feature, so sqlite's C sources are compiled during the build). The reason for
the toolchain floor is in [`docs/01_setup.md`](docs/01_setup.md).

Both paths install the same binary. npm is for "users who don't want to touch
Rust"; `setup.sh` is for "people changing the code" and "people running
unreleased versions".

## Docs

| Goal                                    | Where                                                         |
| --------------------------------------- | ------------------------------------------------------------- |
| Build, run the backend, sign in, debug  | [`docs/01_setup.md`](docs/01_setup.md)                        |
| How sessions are stored locally         | [`docs/02_session_store.md`](docs/02_session_store.md)        |
| The terminal interface for humans       | [`docs/07_tui.md`](docs/07_tui.md)                            |
| Login / token mechanics                 | [`docs/commands/auth.md`](docs/commands/auth.md)              |
| Local low-entropy secret filtering      | [`docs/05_global_secret_filter.md`](docs/05_global_secret_filter.md) |
| Reversible repository secret placeholders | [`docs/06_repository_secret_dictionary.md`](docs/06_repository_secret_dictionary.md) |
| Probed storage formats of each runtime  | [`docs/mechanism-probing/`](docs/mechanism-probing/)          |
| npm package behavior and env vars       | [`npm/README.md`](npm/README.md)                              |
| Release artifact naming contract        | [`.github/RELEASE_ARTIFACTS.md`](.github/RELEASE_ARTIFACTS.md) |
| What changed in each release            | [`CHANGELOG.md`](CHANGELOG.md)                                |

The server (**AgentGit**) is a separate repository, deployed on its own.

## Release

`Cargo.toml` is the single source of the version; every npm manifest must
agree with it (`node scripts/check-version.js` — run in CI and before
`npm pack`). To move the version, change every spot in one shot:

```sh
node scripts/bump-version.mjs 0.1.0    # Cargo.toml + Cargo.lock + all npm manifests
```

Release notes live in [`CHANGELOG.md`](CHANGELOG.md), and they come first: write the
version's `## [x.y.z]` section, then bump. `bump-version.mjs` refuses to touch a file for
a version that has no section, and `check-version.js` refuses one in CI and before `npm pack`.
The section becomes the GitHub Release body (`scripts/release-notes.mjs` extracts it in
`release.yml`, with repository-relative links pinned to the release tag) and ships inside
the `@einsia/agent-git` package.

Pushing the tag runs the whole chain — binaries first, npm right after:

```sh
git tag agit-v0.1.0 && git push origin agit-v0.1.0   # release.yml builds, smoke-tests, attaches artifacts
```

Distribution to users runs through the npm registry: each target ships as a
platform sub-package (`@einsia/agent-git-linux-x64` and friends, gated by
`os`/`cpu` — npm installs only the matching one), and the main
`@einsia/agent-git` pulls them in via `optionalDependencies`. The GitHub
Release holds the canonical tarballs plus `SHA256SUMS`
([contract](.github/RELEASE_ARTIFACTS.md)).

Publishing to npm is the **npm publish** workflow. It runs automatically
once the Release workflow finishes green (and can be dispatched manually —
Actions → npm publish — for retries, `next` dist-tag trials, or a `dry_run`).
It publishes the whole family in dependency order — platform packages →
`@einsia/agent-git` → `create-agit` — with a real global install of the main
tarball as a preflight in between, authenticating via npm trusted publishing
(OIDC, no stored token). The equivalent local fallback:

```sh
gh release download agit-v0.1.0 -p 'agit-*.tar.gz' -D /tmp/agit-dist
node npm/publish.mjs /tmp/agit-dist        # platform packages → main → create-agit
```

`npm/create-agit/` is the one-shot installer behind `npx -y create-agit` — it
depends on the main package and turns the npx cache copy into a durable
`~/.local/bin/agit`, then wires skills/hooks/MCP via `agit setup`. There is
deliberately no unscoped alias package: `agit` on npm belongs to an unrelated
project, and npm's typosquat rule blocks `agent-git` for being one punctuation
mark away from a third party's `agentgit`.

The version oracle (`GET /api/cli/version` on the hub) and `agit upgrade`
both read the npm registry; integrity verification uses the registry's
SRI (sha512). Self-hosted hubs that pin an internal fork set
`AGIT_BACKEND_CLI_REPO` and the whole chain reverts to the GitHub path.

`@einsia/agent-git` is a fresh package name with no existing users, so the
first release goes straight to `latest` — no need to hide in `next` first.
(The old `@einsia/agentgit` stays put; it points at the pre-rewrite CLI.)
