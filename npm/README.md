# @einsia/agentgit (npm distribution)

This package installs the prebuilt **`agit`** CLI (and its companion
**`agit-hub`** server) so you can get them via npm instead of building from
source or running `cargo install`.

```sh
npm install -g @einsia/agentgit
agit --help
```

`agit` is a native Rust binary. This package is a thin installer: on `npm
install` a `postinstall` script downloads the release archive that matches your
platform from GitHub Releases, verifies its SHA256 against the release's
`SHA256SUMS`, and unpacks the binaries. The `agit` / `agit-hub` commands are
small Node shims that `exec` the real binary, forwarding arguments, stdio, and
the exit code unchanged.

## Supported platforms

| OS      | Arch          | Target triple                 |
| ------- | ------------- | ----------------------------- |
| Linux   | x86_64        | `x86_64-unknown-linux-gnu`    |
| Linux   | aarch64/arm64 | `aarch64-unknown-linux-gnu`   |
| macOS   | x86_64        | `x86_64-apple-darwin`         |
| macOS   | arm64         | `aarch64-apple-darwin`        |
| Windows | x86_64        | `x86_64-pc-windows-msvc`      |

Installing on any other platform fails loudly at `postinstall`; build from
source instead.

## Verification & failure behaviour

- **Checksum mismatch is fatal.** The archive's SHA256 must match the entry in
  `SHA256SUMS`, or installation aborts without writing any binary — a mismatch
  is treated as a possible supply-chain compromise, never a warning.
- **Download failures exit non-zero** with the URL and underlying error, so a
  failed install never silently leaves you without a working `agit`.
- Unpacking uses a built-in tar reader on Unix (no dependency on system `tar`);
  Windows zips are unpacked with the OS-provided `tar`/`Expand-Archive`.

## Environment variables

| Variable                  | Effect                                                                                 |
| ------------------------- | -------------------------------------------------------------------------------------- |
| `AGIT_BINARY`             | Absolute path to a prebuilt `agit`; used as-is (no download, no checksum). Honoured by both the installer and the run-time shim. |
| `AGIT_HUB_BINARY`         | Same, for `agit-hub`.                                                                   |
| `AGIT_SKIP_DOWNLOAD`      | Skip the postinstall download (exit 0). For airgapped/CI installs that provide the binary another way. |
| `AGIT_DOWNLOAD_BASE_URL`  | Override the base URL the archive + `SHA256SUMS` are fetched from (e.g. an internal mirror). |
| `AGIT_REPO`               | Override the GitHub `owner/repo` used to build the download URL.                        |
| `AGIT_DOWNLOAD_TIMEOUT_MS`| Per-request timeout in milliseconds (default `60000`).                                  |

Standard proxy variables are honoured for the download:
`HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` and their npm equivalents
(`npm_config_https_proxy`, `npm_config_proxy`).

### Installing behind `--ignore-scripts`

`npm install --ignore-scripts` skips the postinstall, so no binary is
downloaded. The `agit` command then prints how to recover: rerun `npm install`,
run the installer manually (`node .../npm/install.js`), or point `AGIT_BINARY`
at a local build.

## Offline / CI

Point `AGIT_BINARY` (and optionally `AGIT_HUB_BINARY`) at binaries you already
have, and the installer copies them into place instead of hitting the network:

```sh
AGIT_BINARY=/path/to/agit npm install -g @einsia/agentgit
```
