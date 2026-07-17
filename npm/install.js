#!/usr/bin/env node
'use strict';

// postinstall: detect this platform, download the matching release archive,
// verify its SHA256 against the release's SHA256SUMS, and extract the binaries
// next to the bin shim. Every failure mode exits NON-ZERO with a clear message
// — a silent install that leaves no binary is the worst possible outcome.

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const platform = require('./lib/platform');
const config = require('./lib/config');
const { fetchBuffer } = require('./lib/download');
const { extractArchive } = require('./lib/extract');
const log = require('./lib/log');

const BIN_DIR = path.join(__dirname, 'bin');

function isTruthy(v) {
  return v != null && v !== '' && v !== '0' && String(v).toLowerCase() !== 'false';
}

function fail(lines) {
  for (const line of Array.isArray(lines) ? lines : [lines]) log.error(line);
  process.exit(1);
}

// Copy a caller-provided binary into place instead of downloading.
function installLocal(src, dest, isWindows) {
  if (!fs.existsSync(src)) fail(`local binary not found: ${src}`);
  fs.copyFileSync(src, dest);
  if (!isWindows) fs.chmodSync(dest, 0o755);
}

function finalizeBinary(p, isWindows, { required }) {
  if (!fs.existsSync(p)) {
    if (required) fail(`expected binary missing after extraction: ${p}`);
    return false;
  }
  if (fs.statSync(p).size === 0) fail(`extracted binary is empty: ${p}`);
  if (!isWindows) fs.chmodSync(p, 0o755);
  return true;
}

// Find the expected hash for `name` in a GNU coreutils SHA256SUMS body.
function parseSums(text, name) {
  for (const line of text.split(/\r?\n/)) {
    const m = line.match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (!m) continue;
    const file = m[2].trim();
    if (file === name || file.split('/').pop() === name) return m[1];
  }
  return null;
}

async function main() {
  // Deliberate opt-out for airgapped/CI setups that provide the binary another
  // way. Exit 0 (a chosen skip), but be loud that the command is not yet usable.
  if (isTruthy(process.env.AGIT_SKIP_DOWNLOAD)) {
    log.warn('AGIT_SKIP_DOWNLOAD is set — skipping the binary download.');
    log.warn('`agit` will not run until a binary is provided via AGIT_BINARY or you reinstall.');
    return;
  }

  let info;
  try {
    info = platform.detect();
  } catch (e) {
    fail([
      e.message,
      `Supported targets: ${platform.supportedList()}.`,
      `Build from source instead: ${config.sourceUrl()}`,
    ]);
  }

  fs.mkdirSync(BIN_DIR, { recursive: true });

  const primaryDest = path.join(BIN_DIR, info.primaryBinary);
  const hubDest = path.join(BIN_DIR, info.hubBinary);

  // Local-binary override: skip the network entirely.
  if (process.env.AGIT_BINARY) {
    installLocal(process.env.AGIT_BINARY, primaryDest, info.isWindows);
    if (process.env.AGIT_HUB_BINARY) {
      installLocal(process.env.AGIT_HUB_BINARY, hubDest, info.isWindows);
    }
    log.info(`Using AGIT_BINARY (${process.env.AGIT_BINARY}); skipped download.`);
    return;
  }

  const version = config.version;
  const archive = platform.archiveName(version, info.target, info.ext);
  const base = config.releaseBaseUrl();
  const archiveUrl = `${base}/${archive}`;
  const sumsUrl = `${base}/${config.CHECKSUMS_FILE}`;

  log.info(`Installing agit v${version} for ${info.target}`);
  log.info(`Downloading ${archiveUrl}`);

  let archiveBuf;
  try {
    archiveBuf = await fetchBuffer(archiveUrl);
  } catch (e) {
    fail([
      `download failed: ${archiveUrl}`,
      `  ${e.message}`,
      `If you are offline or behind a proxy, set a proxy (HTTPS_PROXY) or provide a local`,
      `binary with AGIT_BINARY=/path/to/agit. To build from source: ${config.sourceUrl()}`,
    ]);
  }

  let sumsText;
  try {
    sumsText = (await fetchBuffer(sumsUrl)).toString('utf8');
  } catch (e) {
    fail([`could not download checksums: ${sumsUrl}`, `  ${e.message}`]);
  }

  // Verify BEFORE touching disk. A mismatch is a supply-chain red flag, not a
  // warning — refuse hard.
  const expected = parseSums(sumsText, archive);
  if (!expected) {
    fail([
      `${config.CHECKSUMS_FILE} has no entry for ${archive}.`,
      'Refusing to install an unverified binary.',
    ]);
  }
  const actual = crypto.createHash('sha256').update(archiveBuf).digest('hex');
  if (actual.toLowerCase() !== expected.toLowerCase()) {
    fail([
      'SHA256 CHECKSUM MISMATCH — refusing to install.',
      `  archive:  ${archive}`,
      `  expected: ${expected}`,
      `  actual:   ${actual}`,
      'This can mean a corrupted download or a supply-chain tampering attempt.',
    ]);
  }
  log.info(`Checksum verified (sha256 ${actual.slice(0, 16)}...)`);

  let extracted;
  try {
    extracted = extractArchive(archiveBuf, info, BIN_DIR);
  } catch (e) {
    fail([`failed to extract ${archive}: ${e.message}`]);
  }
  if (!extracted.includes(info.primaryBinary)) {
    fail([`archive ${archive} did not contain ${info.primaryBinary}.`]);
  }

  finalizeBinary(primaryDest, info.isWindows, { required: true });
  const gotHub = finalizeBinary(hubDest, info.isWindows, { required: false });
  if (!gotHub) {
    log.warn(`${info.hubBinary} was not in the archive; the agit-hub command will be unavailable.`);
  }

  log.info(`agit v${version} installed -> ${primaryDest}`);
}

main().catch((e) => {
  log.error(e && e.stack ? e.stack : String(e));
  process.exit(1);
});
