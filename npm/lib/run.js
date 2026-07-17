'use strict';

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const platform = require('./platform');
const log = require('./log');

// Where the postinstall drops the extracted binaries.
function binDir() {
  return path.join(__dirname, '..', 'bin');
}

// Resolve the real binary to exec for a logical command name ('agit' | 'agit-hub').
// Order: explicit env override, then the installed binary. Returns null if
// nothing usable is found (caller turns that into a loud, actionable error).
function resolveBinary(name) {
  const overrideEnv = name === 'agit-hub' ? 'AGIT_HUB_BINARY' : 'AGIT_BINARY';
  const override = process.env[overrideEnv];
  if (override) {
    if (!fs.existsSync(override)) {
      log.error(`${overrideEnv} points at "${override}" but no file exists there.`);
      process.exit(1);
    }
    return override;
  }

  let fileName;
  try {
    const info = platform.detect();
    fileName = name === 'agit-hub' ? info.hubBinary : info.primaryBinary;
  } catch (e) {
    // Unknown platform: fall back to a best-effort name so we can still find a
    // binary someone dropped in manually, rather than crashing here.
    fileName = process.platform === 'win32' ? `${name}.exe` : name;
  }

  const p = path.join(binDir(), fileName);
  return fs.existsSync(p) ? p : null;
}

// Exec the resolved binary, forwarding argv and stdio and reproducing its exit
// status EXACTLY — agit is a git wrapper, so exit codes and stdio passthrough
// are load-bearing.
function run(name) {
  const bin = resolveBinary(name);
  if (!bin) {
    const overrideEnv = name === 'agit-hub' ? 'AGIT_HUB_BINARY' : 'AGIT_BINARY';
    const installer = path.join(__dirname, '..', 'install.js');
    log.error(`the "${name}" binary is not installed.`);
    log.error('The postinstall download was skipped (e.g. `npm install --ignore-scripts`) or failed.');
    log.error('Resolve it with one of:');
    log.error('  - reinstall without skipping scripts:   npm install');
    log.error(`  - run the installer manually:            node "${installer}"`);
    log.error(`  - point at a local binary:               ${overrideEnv}=/absolute/path/to/${name}`);
    process.exit(127);
  }

  const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });

  if (result.error) {
    if (result.error.code === 'ENOENT') {
      log.error(`could not execute ${bin}: file not found.`);
    } else if (result.error.code === 'EACCES') {
      log.error(`could not execute ${bin}: permission denied (is it executable?).`);
    } else {
      log.error(`could not execute ${bin}: ${result.error.message}`);
    }
    process.exit(127);
  }

  // Killed by a signal: re-raise it so our exit reflects the same cause.
  if (result.signal) {
    process.kill(process.pid, result.signal);
    // If the signal did not terminate us, mirror the shell convention.
    process.exit(1);
  }

  process.exit(result.status === null ? 1 : result.status);
}

module.exports = { run, resolveBinary, binDir };
