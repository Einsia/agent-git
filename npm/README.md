# @einsia/agent-git (npm distribution)

This package ships the prebuilt **`agit`** CLI — no Rust toolchain needed.

```sh
npx -y create-agit@latest          # one-shot install: binary + skills/hooks/MCP
# or as a global package:
npm install -g @einsia/agent-git --allow-scripts=@einsia/agent-git
agit --help
```

Two package names, one CLI:

| Package             | Role                                                        |
| ------------------- | ----------------------------------------------------------- |
| `@einsia/agent-git` | the real package — shim, postinstall, platform sub-packages |
| `create-agit`       | `npx` one-shot installer, lands `agit` in `~/.local/bin`    |

> The old package `@einsia/agentgit` (no hyphen) is the pre-rewrite CLI. This
> package is its successor, not a new version of it; there is no upgrade path
> between the two.

`agit` is a native Rust binary. This package is a thin wrapper: npm installs
the platform sub-package matching your `os`/`cpu` via `optionalDependencies`
(`@einsia/agent-git-linux-x64` and friends), the `agit` bin is a Node shim
that execs that binary with argv/stdio/exit-code forwarded, and on a global install
`postinstall` runs `agit setup` once to wire skills, hooks and MCP; it writes no AGENTS.md
into any folder (`agit setup --agents-md` adds the section to a project on request). A local
install (a project dependency, or `npm install --prefix <dir>`) and `npx` configure nothing
and record no installation: their binary goes away with that folder, so run `agit setup`
yourself from a durable install if you want the integrations.

The executable contains Git and Git LFS. On first use it extracts them under
`$AGIT_HOME/git-runtime` (default `~/.agit/git-runtime`), with no runtime download.
Only AgentGit's child processes use that private runtime; your shell PATH and
global Git configuration are unchanged. Copying or upgrading the single executable
also carries its Git runtime. External credential helpers and extensions may still
need your local tools; `AGIT_USE_SYSTEM_GIT=1` uses your system Git installation.

The global package wires skills, hooks and MCP from its install script. npm 12 and later
skip an install script that has not been allowed, which is what `--allow-scripts` above is for;
an organization policy (`strict-allow-scripts`, `ignore-scripts`) can block or skip it too. Under
pnpm (`pnpm add -g @einsia/agent-git`) the install script configures nothing. If the install
script did not run, run `agit setup` once yourself.

The hub (**AgentGit**) is not in this package; it deploys separately.

## Platforms with prebuilt binaries

| OS    | Arch          | Platform package                  |
| ----- | ------------- | --------------------------------- |
| Linux | x86_64        | `@einsia/agent-git-linux-x64`     |
| Linux | aarch64/arm64 | `@einsia/agent-git-linux-arm64`   |
| macOS | x86_64        | `@einsia/agent-git-darwin-x64`    |
| macOS | arm64         | `@einsia/agent-git-darwin-arm64`  |
| Windows | x86_64      | `@einsia/agent-git-win32-x64`    |

**Linux binaries are musl static-linked — no glibc floor.** Verified on Alpine,
Amazon Linux 2 (glibc 2.26), Debian 11/12, Ubuntu 20.04–24.04, x86_64 and
arm64. No more `libc.so.6: version 'GLIBC_2.xx' not found`.

On macOS, if Node runs under Rosetta (it reports itself as x64) the installer
detects it and installs the arm64 build.

Unsupported operating system and architecture pairs get no prebuilt binary: running
`agit` prints build-from-source instructions. On WSL, the Linux binary works.

## Failure behavior

- Unsupported platform → the shim explains and exits 127 with the source-build
  recipe.
- `agit setup` failing in postinstall never blocks the install — the CLI is on
  disk; re-run `agit setup` any time.
- If npm skips the install script (npm 12 and later without `--allow-scripts`,
  `--ignore-scripts`, or a policy that blocks it), the CLI is installed but no
  skills, hooks or MCP are wired: run `agit setup` once.
- An agit that was never set up says so when a command that starts or records capture
  runs (`on`, `save`, `import`, `new`, `resume`, `run`, `commit`, and `login`). A
  terminal a person answers is asked once whether to set it up now, after a list of what
  that writes; No is the default and is remembered. Any other caller, such as an agent,
  a script or CI, gets one line a day and nothing is written outside `AGIT_HOME`. Reads
  never show it. `agit doctor` reports the state, including a decline.
- If npm suppresses postinstall, the next invocation of a globally installed
  `agit` schedules a guarded RC daemon reconciliation in the background.
  npm cannot run the daemon handoff at install time when it suppresses scripts.

## Environment variables

| Variable         | Effect                                                              |
| ---------------- | ------------------------------------------------------------------- |
| `AGIT_BINARY`    | Point at an existing `agit` binary; shim and postinstall honor it.  |
| `AGIT_SKIP_SETUP`| Skip the postinstall `agit setup`, and the notice that agit is not set up (CI, managed environments). |
| `AGIT_FORCE_INSTALL` | Also run postinstall inside the source checkout (CI smoke tests). |
| `AGIT_HUB_URL`   | Point the CLI at another hub (default `https://agent-git.com`).  |
| `AGIT_NPM_REGISTRY` | Registry `agit upgrade` downloads platform packages from.        |
| `AGIT_USE_SYSTEM_GIT` | Set to `1` to use system Git and Git LFS instead of the bundled runtime. |

## Upgrading

`agit upgrade` asks the hub for the latest CLI version and downloads the
platform package tarball straight from the npm registry, verified against the
registry's SRI (sha512) before anything is replaced.

Windows packages contain `agit.exe`. The `create-agit` installer writes it to
`%USERPROFILE%\.local\bin`; add that directory to your user PATH if the installer
reports that it is missing. Windows source builds need Rust and the MSVC C/C++
toolchain. The local RC daemon uses a current-user Windows named pipe and private
RC state. It refuses shared or redirected RC paths instead of changing their permissions.
