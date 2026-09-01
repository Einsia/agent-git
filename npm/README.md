# @einsia/agent-git (npm distribution)

This package ships the prebuilt **`agit`** CLI — no Rust toolchain needed.

```sh
npx -y create-agit                 # one-shot install: binary + skills/hooks/MCP
# or as a global package:
npm install -g @einsia/agent-git   # pnpm add -g @einsia/agent-git — same CLI
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
that execs that binary with argv/stdio/exit-code forwarded, and `postinstall`
runs `agit setup` once to wire skills, hooks, MCP and AGENTS.md.

pnpm (v10+) blocks dependency install scripts unless approved, so under pnpm
the automatic `agit setup` does not run — run it once yourself after
installing (or `pnpm approve-builds -g` and reinstall).

The hub (**AgentGit**) is not in this package; it deploys separately.

## Platforms with prebuilt binaries

| OS    | Arch          | Platform package                  |
| ----- | ------------- | --------------------------------- |
| Linux | x86_64        | `@einsia/agent-git-linux-x64`     |
| Linux | aarch64/arm64 | `@einsia/agent-git-linux-arm64`   |
| macOS | x86_64        | `@einsia/agent-git-darwin-x64`    |
| macOS | arm64         | `@einsia/agent-git-darwin-arm64`  |

**Linux binaries are musl static-linked — no glibc floor.** Verified on Alpine,
Amazon Linux 2 (glibc 2.26), Debian 11/12, Ubuntu 20.04–24.04, x86_64 and
arm64. No more `libc.so.6: version 'GLIBC_2.xx' not found`.

On macOS, if Node runs under Rosetta (it reports itself as x64) the installer
detects it and installs the arm64 build.

Unsupported platforms (including Windows) get no prebuilt binary: running
`agit` prints build-from-source instructions. On WSL, the Linux binary works.

## Failure behavior

- Unsupported platform → the shim explains and exits 127 with the source-build
  recipe.
- `agit setup` failing in postinstall never blocks the install — the CLI is on
  disk; re-run `agit setup` any time.

## Environment variables

| Variable         | Effect                                                              |
| ---------------- | ------------------------------------------------------------------- |
| `AGIT_BINARY`    | Point at an existing `agit` binary; shim and postinstall honor it.  |
| `AGIT_SKIP_SETUP`| Skip the postinstall `agit setup` (CI, managed environments).       |
| `AGIT_FORCE_INSTALL` | Also run postinstall inside the source checkout (CI smoke tests). |
| `AGIT_HUB_URL`   | Point the CLI at another hub (default `https://agent-git.com`).  |
| `AGIT_NPM_REGISTRY` | Registry `agit upgrade` downloads platform packages from.        |

## Upgrading

`agit upgrade` asks the hub for the latest CLI version and downloads the
platform package tarball straight from the npm registry, verified against the
registry's SRI (sha512) before anything is replaced.
