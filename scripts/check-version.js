#!/usr/bin/env node
'use strict';

// The version in Cargo.toml and the version in package.json must match.
//
// A mismatch is not "something renders badly": postinstall builds the download URL from the
// package.json version, while the version inside the artifact filename comes from Cargo.toml.
// One digit apart and npm installs a 404 — or worse, a binary of a different version, while
// `agit --version` names a third one.
//
// CI runs it (packaging:npm in .gitlab-ci.yml), and so does every `npm pack` / `npm publish`
// (prepack in package.json), so drift cannot last long.
//
//   node scripts/check-version.js

const fs = require('fs');
const path = require('path');

const root = path.join(__dirname, '..');
const cargoPath = path.join(root, 'Cargo.toml');
const pkgPath = path.join(root, 'package.json');

// A published tarball carries no Cargo.toml. prepack runs inside the repository, so it is
// normally there; when it is missing, skip rather than blow up a consumer-side scenario here.
if (!fs.existsSync(cargoPath)) {
  console.error('[check-version] no Cargo.toml, skipping.');
  process.exit(0);
}

// Reads only the `version` key of the [package] section. No toml parser: the npm side of this
// repository is dependency-free, and one line of regex does not earn an exception. Every
// dependency version = "..." sits after the [package] section, and this stops at the next
// section header, so it cannot misread one.
function cargoVersion(text) {
  let inPackage = false;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith('[')) {
      inPackage = line === '[package]';
      continue;
    }
    if (!inPackage) continue;
    const m = line.match(/^version\s*=\s*"([^"]+)"/);
    if (m) return m[1];
  }
  return null;
}

const cargo = cargoVersion(fs.readFileSync(cargoPath, 'utf8'));
const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8')).version;

if (!cargo) {
  console.error('[check-version] no version in the [package] section of Cargo.toml.');
  process.exit(1);
}

if (cargo !== pkg) {
  console.error('[check-version] version mismatch:');
  console.error(`  Cargo.toml   ${cargo}`);
  console.error(`  package.json ${pkg}`);
  console.error('set both to the same value before committing.');
  process.exit(1);
}

// The platform subpackages and the install shim ship in the same release: a version
// mismatch means optionalDependencies points at nothing, which is not a failed install but
// "installed, and running agit says there is no prebuilt binary for your platform".
const sibPkgs = [
  'npm/create-agit/package.json',
  'npm/platforms/linux-x64/package.json',
  'npm/platforms/linux-arm64/package.json',
  'npm/platforms/darwin-x64/package.json',
  'npm/platforms/darwin-arm64/package.json',
];
for (const rel of sibPkgs) {
  const full = path.join(root, rel);
  if (!fs.existsSync(full)) continue; // a published tarball does not carry these
  const pkg = JSON.parse(fs.readFileSync(full, 'utf8'));
  if (pkg.version !== cargo) {
    console.error(`[check-version] ${rel} is ${pkg.version}, the main version is ${cargo}.`);
    console.error('bump the version with `node scripts/bump-version.mjs <version>`; one run syncs every place it lands.');
    process.exit(1);
  }
  // An install shim pins the main package to an exact version; pinned to any other version, the
  // shim and the CLI it installs are not from the same release.
  const pin = pkg.dependencies?.['@einsia/agent-git'];
  if (pin !== undefined && pin !== cargo) {
    console.error(`[check-version] ${rel} pins @einsia/agent-git@${pin}, the main version is ${cargo}.`);
    process.exit(1);
  }
}

// stderr, like every branch above. This script hangs off prepack, and prepack's stdout is the
// same stream as `npm pack`'s own output: with `--json` that is JSON somebody parses, and with
// no arguments it is the tarball filename. A log line inserted there breaks both.
console.error(`[check-version] ok: ${cargo}`);
