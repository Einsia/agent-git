'use strict';

const { spawnSync } = require('child_process');
const path = require('path');
const platform = require('./platform');
const { resolveBinary } = require('./resolve');
const log = require('./log');

// Entry point for the `agit` command. Thin is deliberate: forward argv / stdio /
// exit code, nothing else. agit gets written into scripts and hooks, so exit
// codes and stdio pass-through are load-bearing semantics and must not change.
function run() {
  const pkgRoot = path.join(__dirname, '..');
  let bin = null;
  try {
    bin = resolveBinary(pkgRoot);
  } catch (e) {
    log.error(e.message);
    process.exit(1);
  }

  if (!bin) {
    // npm skipped the optional dep for this os/cpu = this platform has no
    // prebuilt artifact (win32, freebsd, ...). This block is often everything
    // the user sees, so on its own it has to say why there is no binary and
    // which command fixes it.
    log.error(`no prebuilt agit binary for ${process.platform}/${process.arch}.`);
    log.error('');
    log.error(`prebuilt: ${platform.supportedList()}`);
    log.error('');
    log.error('build from source instead:');
    log.error('  git clone https://github.com/Einsia/agent-git');
    log.error('  cd agent-git && ./setup.sh');
    log.error('');
    log.error('already have a binary? AGIT_BINARY=/path/to/agit agit …');
    // 127 is the shell's conventional code for "command not found".
    process.exit(127);
  }

  const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
  if (r.error) {
    log.error(`failed to start ${bin}: ${r.error.message}`);
    process.exit(1);
  }
  process.exit(r.status === null ? 1 : r.status);
}

module.exports = { run };
