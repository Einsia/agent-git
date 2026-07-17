# Release artifacts

This is the stable contract between the release workflow (`.github/workflows/release.yml`) and any
downstream consumer (notably the npm package). If the release workflow changes what it produces,
this file and the npm packaging must change together.

## Trigger

A release is built by pushing a tag matching `v*`:

```
git tag v0.1.0
git push origin v0.1.0
```

`VERSION` is the tag name with the leading `v` stripped: tag `v0.1.0` → `VERSION = 0.1.0`.

## Targets and archive names

One archive is published per target, attached to the GitHub Release for the tag:

| Target triple                    | Runner         | Archive name                                        |
| -------------------------------- | -------------- | --------------------------------------------------- |
| `x86_64-unknown-linux-gnu`       | ubuntu-latest  | `agit-<VERSION>-x86_64-unknown-linux-gnu.tar.gz`    |
| `aarch64-unknown-linux-gnu`      | ubuntu-latest  | `agit-<VERSION>-aarch64-unknown-linux-gnu.tar.gz`   |
| `x86_64-apple-darwin`            | macos-latest   | `agit-<VERSION>-x86_64-apple-darwin.tar.gz`         |
| `aarch64-apple-darwin`           | macos-latest   | `agit-<VERSION>-aarch64-apple-darwin.tar.gz`        |
| `x86_64-pc-windows-msvc`         | windows-latest | `agit-<VERSION>-x86_64-pc-windows-msvc.zip`         |

Naming scheme, stated once so it can be relied on programmatically:

```
agit-<VERSION>-<TARGET_TRIPLE>.tar.gz     # linux, macos
agit-<VERSION>-<TARGET_TRIPLE>.zip         # windows
```

## Archive contents

Each archive contains **both** binaries, at the **root** of the archive (no wrapping directory):

- unix (`.tar.gz`): `agit`, `agit-hub`
- windows (`.zip`): `agit.exe`, `agit-hub.exe`

So a consumer extracts the archive and finds the executable directly (e.g. `./agit`,
`./agit-hub.exe`) — there is no `agit-<VERSION>-<target>/` prefix to strip.

## Checksums

A single `SHA256SUMS` file is also attached to the Release. It lists every archive by bare filename
in standard `sha256sum` / `shasum -a 256` format:

```
<sha256>  agit-<VERSION>-x86_64-unknown-linux-gnu.tar.gz
<sha256>  agit-<VERSION>-aarch64-unknown-linux-gnu.tar.gz
...
```

Verify (from a directory containing both the archive and `SHA256SUMS`):

```
sha256sum -c SHA256SUMS --ignore-missing     # linux
shasum -a 256 -c SHA256SUMS --ignore-missing # macos
```

## Download URLs

Release assets follow GitHub's standard pattern:

```
https://github.com/<owner>/<repo>/releases/download/v<VERSION>/agit-<VERSION>-<TARGET_TRIPLE>.tar.gz
https://github.com/<owner>/<repo>/releases/download/v<VERSION>/SHA256SUMS
```

(The tag in the URL keeps its `v`; only the file name uses the stripped `VERSION`.)
