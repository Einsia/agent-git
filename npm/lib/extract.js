'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const zlib = require('zlib');
const { spawnSync } = require('child_process');

// Extract the wanted binaries from a release archive held in memory.
//   info: the object returned by platform.detect()
// Returns the list of basenames actually written into destDir.
function extractArchive(archiveBuf, info, destDir) {
  if (info.isWindows) {
    return extractZip(archiveBuf, info, destDir);
  }
  return extractTarGz(archiveBuf, info.binaries, destDir);
}

// --- tar.gz --------------------------------------------------------------
// The archive holds a couple of short-named regular files (`agit`, `agit-hub`).
// That is squarely inside the plain ustar format, so a tiny reader beats
// depending on a system `tar` or an npm tarball library. Files are matched by
// basename and written flat into destDir.
function extractTarGz(gzBuf, wanted, destDir) {
  const buf = zlib.gunzipSync(gzBuf);
  const wantSet = new Set(wanted);
  const extracted = [];

  let offset = 0;
  while (offset + 512 <= buf.length) {
    const header = buf.subarray(offset, offset + 512);
    if (isZeroBlock(header)) break; // end-of-archive marker

    let name = readString(header, 0, 100);
    const prefix = readString(header, 345, 155); // ustar path prefix
    if (prefix) name = prefix + '/' + name;

    const size = parseInt(readString(header, 124, 12).trim() || '0', 8) || 0;
    const typeflag = header[156]; // '0' (0x30) or NUL (0x00) => regular file
    const dataStart = offset + 512;
    const base = name.split('/').pop();

    const isRegular = typeflag === 0x30 || typeflag === 0;
    if (isRegular && (wantSet.has(base) || wantSet.has(name))) {
      const data = buf.subarray(dataStart, dataStart + size);
      fs.writeFileSync(path.join(destDir, base), data);
      extracted.push(base);
    }

    offset = dataStart + Math.ceil(size / 512) * 512;
  }
  return extracted;
}

function isZeroBlock(block) {
  for (let i = 0; i < block.length; i++) {
    if (block[i] !== 0) return false;
  }
  return true;
}

function readString(block, start, len) {
  let end = start;
  const limit = start + len;
  while (end < limit && block[end] !== 0) end++;
  return block.toString('utf8', start, end);
}

// --- zip (windows only) --------------------------------------------------
// Zip's deflate + central directory is more than we want to hand-roll, so lean
// on tooling that ships with Windows 10+: bsdtar (`tar`) reads zip, and
// Expand-Archive is the PowerShell fallback. destDir is then flattened so the
// binary basenames sit directly in it, matching the tar.gz path.
function extractZip(zipBuf, info, destDir) {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'agit-zip-'));
  const zipPath = path.join(tmpDir, 'agit.zip');
  fs.writeFileSync(zipPath, zipBuf);
  const stageDir = path.join(tmpDir, 'stage');
  fs.mkdirSync(stageDir, { recursive: true });

  try {
    let ok = tryRun('tar', ['-xf', zipPath, '-C', stageDir]);
    if (!ok) {
      ok = tryRun('powershell', [
        '-NoProfile',
        '-Command',
        `Expand-Archive -LiteralPath '${zipPath}' -DestinationPath '${stageDir}' -Force`,
      ]);
    }
    if (!ok) {
      throw new Error('could not extract zip: neither `tar` nor PowerShell Expand-Archive succeeded');
    }

    const extracted = [];
    for (const wantName of info.binaries) {
      const found = findFile(stageDir, wantName);
      if (found) {
        fs.copyFileSync(found, path.join(destDir, wantName));
        extracted.push(wantName);
      }
    }
    return extracted;
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

function tryRun(cmd, args) {
  const r = spawnSync(cmd, args, { stdio: 'ignore' });
  return !r.error && r.status === 0;
}

function findFile(root, name) {
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch (e) {
      continue;
    }
    for (const ent of entries) {
      const full = path.join(dir, ent.name);
      if (ent.isDirectory()) stack.push(full);
      else if (ent.name === name) return full;
    }
  }
  return null;
}

module.exports = { extractArchive, extractTarGz };
