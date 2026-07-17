'use strict';

// ===========================================================================
//  RELEASE-ARTIFACT CONTRACT (single source of truth)
// ---------------------------------------------------------------------------
//  Reconciled against `.github/RELEASE_ARTIFACTS.md`, the stable contract the
//  release workflow publishes. If that contract ever changes, THIS FILE is the
//  one and only place the npm side has to change with it.
//
//  Scheme (per `v*` tag):
//    • Host:        GitHub Releases.
//    • Tag:         `v<version>`                       e.g.  v0.1.0
//    • Per target:  agit-<version>-<target>.tar.gz     (unix, gzip'd tar)
//                   agit-<version>-<target>.zip        (windows)
//                   each archive contains BOTH binaries at its ROOT (no
//                   wrapping dir): `agit`+`agit-hub` / `agit.exe`+`agit-hub.exe`.
//    • Checksums:   a single `SHA256SUMS` asset listing every archive by bare
//                   filename (`<sha256>␠␠<filename>`, GNU coreutils format).
//    • <version>:   the BARE Cargo/npm version in the filename (no `v`);
//                   the `v` prefix appears only in the git tag / URL path.
//    • Targets:     x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
//                   x86_64-apple-darwin, aarch64-apple-darwin,
//                   x86_64-pc-windows-msvc   (see lib/platform.js)
//    • URL:         https://github.com/<owner>/<repo>/releases/download/
//                     v<version>/agit-<version>-<target>.tar.gz  (and /SHA256SUMS)
//
//  The GitHub `owner/repo` that hosts the Releases is `Einsia/agent-git`
//  (the git remote that pushes there). It (and the whole base URL) can be
//  overridden at install time via env vars — see each constant.
// ===========================================================================

const pkg = require('../../package.json');

// GitHub `owner/repo` that hosts the Releases.
//   Override:  AGIT_REPO=myorg/myrepo
const REPO_SLUG = process.env.AGIT_REPO || 'Einsia/agent-git';

// Prefix on the git tag / release path (tag = `${TAG_PREFIX}${version}`).
const TAG_PREFIX = 'v';

// Name of the checksums asset in the release.
const CHECKSUMS_FILE = 'SHA256SUMS';

// The tool version (drives the tag, the URL, and the archive filename).
const version = pkg.version;

function stripTrailingSlash(s) {
  return s.replace(/\/+$/, '');
}

// Base URL that the archive + checksums live directly under.
//   Override the whole base (e.g. an internal mirror):
//     AGIT_DOWNLOAD_BASE_URL=https://mirror.example.com/agit/v0.1.0
function releaseBaseUrl() {
  if (process.env.AGIT_DOWNLOAD_BASE_URL) {
    return stripTrailingSlash(process.env.AGIT_DOWNLOAD_BASE_URL);
  }
  return `https://github.com/${REPO_SLUG}/releases/download/${TAG_PREFIX}${version}`;
}

function sourceUrl() {
  return `https://github.com/${REPO_SLUG}`;
}

module.exports = {
  REPO_SLUG,
  TAG_PREFIX,
  CHECKSUMS_FILE,
  version,
  releaseBaseUrl,
  sourceUrl,
};
