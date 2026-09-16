# Bundled Git runtime

Prebuilt AgentGit distributions embed Git and Git LFS inside the CLI executable.
Source builds keep using system Git unless the `bundled-git` feature is enabled.
The existing npm, one-shot installation, and self-update paths can continue
copying one file.

## Runtime selection and configuration

On startup, the CLI extracts the payload into
`$AGIT_HOME/git-runtime/<payload-sha256>`, defaulting to `~/.agit/git-runtime`.
Extraction uses a private staging directory and a per-payload lock before
publishing the complete directory. Subsequent invocations reuse it; payloads from
other versions remain available to running processes. No runtime download occurs
on the user's machine.

Git subprocesses select the private executable and helpers. Their PATH includes
the private runtime ahead of the inherited PATH, and their `GIT_EXEC_PATH` selects
the matching helpers. The parent shell environment is unchanged. Global and
repository configuration continue to apply; the existing isolated publication
commands retain their restricted configuration environment. LFS configuration
is installed locally in AgentGit repositories.

`agit doctor` reports whether Git is bundled or system, along with the Git and
LFS versions. Set `AGIT_USE_SYSTEM_GIT=1` to use system Git/LFS when local plugins,
credential helpers, or an administrator-managed Git require that installation.
The bundle does not replace external tools referenced by user configuration,
such as SSH, signing tools, editors, or custom credential helpers.

If extraction fails, check that `AGIT_HOME` is writable and private. To rebuild
a damaged cache, stop processes using that runtime and remove only its hash
directory under `git-runtime`; the next invocation extracts it again. Do not
remove `repos` or other AgentGit state.

## Build a distribution binary

Use Python 3.9 or newer and the Rust prerequisites in [setup](01_setup.md).
For a native macOS ARM64 build:

```sh
python3 scripts/prepare-git-runtime.py \
  aarch64-apple-darwin .cache/runtime-macos-arm64.tar.gz
AGIT_GIT_RUNTIME_ARCHIVE="$PWD/.cache/runtime-macos-arm64.tar.gz" \
  cargo build --locked --release --features bundled-git
```

Use the matching target triple for another platform. `build.rs` checks the
payload's companion `.target` marker against Cargo's target. Packaging scripts
and the release workflow prepare the matching payload automatically.

`scripts/git-runtime-lock.json` pins downloads and license texts by SHA-256.
License texts are vendored under `scripts/git-runtime-licenses` and verified
against those pins, so runtime assembly does not need a license-text service.
Assembly verifies checksums, resolves archive links within the payload, checks
binary architectures, and produces a deterministic compressed archive. The
cache stores verified downloads and can be removed without affecting builds.

Linux uses Alpine Git with its musl loader and dependency libraries, invoked
through wrappers with a private library path. It uses the host `/bin/sh` and
carries CA certificates and Git templates. macOS uses dugite-native's Git and
helpers with optional credential-manager components omitted. Windows retains
MinGit and its runtime dependencies. All targets include the pinned Git LFS
release, `runtime-manifest.json`, `THIRD-PARTY-NOTICES`, and license texts with
source references. Refresh the pins and rerun the platform checks when updating
Git, LFS, certificates, or dependency security fixes.

## Verification

```sh
python3 -m unittest discover -s scripts -p test_git_runtime.py
python3 scripts/smoke-git-runtime.py .cache/runtime-macos-arm64.tar.gz
AGIT_GIT_RUNTIME_ARCHIVE="$PWD/.cache/runtime-macos-arm64.tar.gz" \
  cargo test --locked --features bundled-git --test bundled_git
```

The runtime smoke check stages LFS data, commits, pushes to a local bare remote,
and clones with only private Git helpers on PATH. The CLI integration check
copies the executable to an installation path containing spaces and exercises
LFS commit and restoration with system Git unavailable, while checking that the
user's global Git profile remains unchanged. CI covers native macOS and Windows
and Linux with both glibc and musl hosts; release checks execute the embedded
runtime on the supported Linux distribution matrix.
