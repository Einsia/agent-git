# Release artifact contract

The stable contract between the release workflow (`.github/workflows/release.yml`)
and its downstream consumers (chiefly the npm publish chain). When what the
workflow produces changes, this document, `npm/publish.mjs` and
`npm/lib/platform.js` change with it.

## Trigger

Push an `agit-v*` tag:

```
git tag agit-v0.1.0
git push origin agit-v0.1.0
```

`VERSION` is the tag name with the prefix stripped: `agit-v0.1.0` →
`VERSION = 0.1.0`.

The prefix is `agit-v` and not `v`: `v0.1.0` ... `v0.5.0` in this repository
belong to **the project before the rewrite** and are already pushed. Reusing `v`
hits two traps at once — `git push origin v0.1.0` is rejected outright (the tag
exists), while the npm package's source fallback,
`cargo install --git --tag v0.1.0`, **succeeds** and installs that old code
(observed: it builds the old CLI, `agit-hub` and all, still stamped 0.1.0).
The second is the worse one, because nothing errors.

A Release is only buildable when the tag agrees with the version in
`Cargo.toml` / `package.json` — one workflow step exists to block the mismatch.

When the tag already exists but the production Release workflow fails on the
runner or the network, re-select the same `agit-v*` tag from the manual entry
point of the **Release** workflow in GitHub Actions. The manual entry point takes
no branch and no bare SHA, and creates no second release identity; it retries the
same tag, safely.

## Targets and file names

One archive per target, attached to the GitHub Release of this tag:

| Target triple                | Runner        | Archive name                                       |
| ---------------------------- | ------------- | -------------------------------------------------- |
| `x86_64-unknown-linux-musl`  | ubuntu-latest | `agit-<VERSION>-x86_64-unknown-linux-musl.tar.gz`  |
| `aarch64-unknown-linux-musl` | ubuntu-latest | `agit-<VERSION>-aarch64-unknown-linux-musl.tar.gz` |
| `x86_64-apple-darwin`        | macos-latest  | `agit-<VERSION>-x86_64-apple-darwin.tar.gz`        |
| `aarch64-apple-darwin`       | macos-latest  | `agit-<VERSION>-aarch64-apple-darwin.tar.gz`       |

```
agit-<VERSION>-<TARGET_TRIPLE>.tar.gz
```

**Linux is musl static linking, not gnu.** A gnu artifact turns the build
machine's glibc version into a runtime floor, and GitHub's ubuntu-latest is 24.04
(glibc 2.39): installed from the npm package, the binary reports
`libc.so.6: version 'GLIBC_2.38' not found`, and what that build produces starts
on none of debian:12, ubuntu:22.04, debian:11, amazonlinux:2.

The musl artifact is static-pie, has no interpreter and has **no glibc floor**:
the same file runs on every distribution above plus alpine. The release
workflow's smoke-test step really runs every linux artifact across a container
matrix (alpine / amazonlinux / debian / ubuntu) — see
`Smoke-test the artifact on old distros` in `release.yml`.

Cross-compilation uses `cargo-zigbuild`. Bundled sqlite is real C, so crossing
needs a complete musl cross C toolchain, and apt's `musl-tools` carries only the
x86_64 one; zig ships musl headers and libc for every architecture, so one recipe
covers both architectures at once.

**There is no Windows artifact.** The rustls dependency chain compiles the C code
of aws-lc-sys, which is unstable on a windows runner. On Windows there is no
matching platform package to install; the npm package prints the source-install
instructions at runtime (`npm/lib/run.js`). Installing from source requires the
MSVC C compiler on the machine.

## Archive contents

The archive **root** holds one executable, `agit`, with no wrapping directory —
unpack it and run `./agit` directly, with no path prefix to strip.

(The archive carries no `agit-hub`: the server side is a separate repository and
does not ship with the client.)

## Checksums

The Release carries one more asset, `SHA256SUMS`, listing every archive by bare
file name in `sha256sum` / `shasum -a 256` format:

```
<sha256>  agit-<VERSION>-x86_64-unknown-linux-musl.tar.gz
<sha256>  agit-<VERSION>-aarch64-unknown-linux-musl.tar.gz
...
```

Verify in a directory that holds both the archives and `SHA256SUMS`:

```
sha256sum -c SHA256SUMS --ignore-missing     # linux
shasum -a 256 -c SHA256SUMS --ignore-missing # macos
```

Manual verification and third-party packagers look the bare file name up in this
table; an archive whose checksum does not verify must not be used. The npm side
downloads no Release archive — the platform binaries come from the registry's
platform packages, and the registry's SRI (sha512) guarantees their integrity.

## Download URLs

```
https://github.com/<owner>/<repo>/releases/download/agit-v<VERSION>/agit-<VERSION>-<TARGET_TRIPLE>.tar.gz
https://github.com/<owner>/<repo>/releases/download/agit-v<VERSION>/SHA256SUMS
```

(The tag in the URL path carries `v`; the version in the file name does not.)

## Shipping a version, end to end

1. Fix the version: `node scripts/bump-version.mjs X.Y.Z` — one pass sets
   `Cargo.toml`, `Cargo.lock` and every `package.json` on the npm side (with a
   built-in `check-version` self-check), and commit.
2. Tag and push: `git tag agit-vX.Y.Z && git push origin agit-vX.Y.Z`.
   → `release.yml` cross-compiles every target above, creates the Release, and
   attaches the artifacts and `SHA256SUMS`;
   → once that finishes successfully, the **npm publish** workflow takes over
   automatically through `workflow_run` and publishes the whole family in
   dependency order: the four platform packages → `@einsia/agent-git` →
   `create-agit`, installing the main package's tarball globally for real
   along the way as the preflight.
3. Verify afterwards: install with `npm i -g @einsia/agent-git` (or
   `npx -y create-agit`) and check the version with `agit --version`; check the
   archive checksums as above.

An npm version, once published, is taken forever, so the automatic chain is
backstopped by assertions that run before the publish: tag and version agree,
the artifacts are all present, the binary's version and architecture check out,
and a real install serves as the preflight; an already-published version counts
as an idempotent success, so the whole workflow can be re-run. Publishing a
`next` to test the water, filling in a publish after a manual Release retry, and
running only a dry run all go through the manual entry point of **npm publish**
(Actions → npm publish → fill in tag / dist-tag / dry_run). The equivalent
commands for the local backstop are in the "Release" section of the README.

## One-time setup on the npm side

- Every package name has a **Trusted Publisher** configured on npmjs.com
  (GitHub Actions → `npm-publish.yml` of `Einsia/agent-git`); the workflow
  publishes over OIDC, and the repository holds no long-lived NPM_TOKEN.
- A package name whose first version has never been published has no page to
  configure: the maintainer publishes the first one locally with `npm publish`,
  then hands it back to the workflow.
