'use strict';

const { spawnSync } = require('child_process');

// The current Node runtime → platform key. The main package
// (optionalDependencies), the npx wrapper, publish.mjs and the CLI's
// `agit upgrade` share the same key set, and the key is the suffix of the npm
// platform package name: @einsia/agent-git-<key>. Changing this means changing
// release.yml's matrix, publish.mjs's mapping and upgrade.rs too.
//
// Linux is musl static linking: a gnu artifact built on ubuntu:24.04 locks in
// GLIBC_2.39 and nothing starts on older distributions; musl's static-pie has
// no interpreter, so it has no glibc floor. No win32: the release pipeline
// produces no windows artifacts.
const KEYS = {
  'linux/x64': 'linux-x64',
  'linux/arm64': 'linux-arm64',
  'darwin/x64': 'darwin-x64',
  'darwin/arm64': 'darwin-arm64',
};

// Platform key → Rust target triple (CI names artifact archives by triple).
const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
};

function packageName(key) {
  return `@einsia/agent-git-${key}`;
}

// Under Rosetta, node reports itself as x64. sysctl.proc_translated=1 means
// the real hardware is arm64, so installing the arm64 package avoids the
// translation layer.
function realArch(nodePlatform, nodeArch) {
  const platform = nodePlatform || process.platform;
  const arch = nodeArch || process.arch;
  if (platform !== 'darwin' || arch !== 'x64') return arch;
  const r = spawnSync('sysctl', ['-n', 'sysctl.proc_translated'], { encoding: 'utf8' });
  return !r.error && r.status === 0 && (r.stdout || '').trim() === '1' ? 'arm64' : arch;
}

// Returns this machine's platform key; an unsupported platform (win32, for
// example) returns null and the caller picks the wording.
function packageKey(nodePlatform, nodeArch) {
  const platform = nodePlatform || process.platform;
  const arch = nodeArch === undefined ? realArch() : nodeArch;
  return KEYS[`${platform}/${arch}`] || null;
}

function binaryName(nodePlatform) {
  return (nodePlatform || process.platform) === 'win32' ? 'agit.exe' : 'agit';
}

function supportedList() {
  return Object.values(KEYS)
    .map((k) => `${k} (${TRIPLES[k]})`)
    .join(', ');
}

module.exports = {
  KEYS,
  TRIPLES,
  packageName,
  packageKey,
  realArch,
  binaryName,
  supportedList,
};
