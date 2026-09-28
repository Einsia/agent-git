'use strict';

const { spawnSync } = require('child_process');
const path = require('path');
const platform = require('./platform');
const { resolveBinary } = require('./resolve');
const log = require('./log');
const { scheduleReconciliation } = require('./reconcile');

// Forward argv, stdio, and exit status unchanged. Hooks rely on those semantics;
// daemon reconciliation therefore runs independently of the forwarded command.
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
    // prebuilt artifact (freebsd, ...). This block is often everything
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

  const args = process.argv.slice(2);
  // A bridge may stay open indefinitely, so queue its install check before forwarding.
  // Other commands finish first to avoid racing their own daemon operation.
  if (args[0] === 'rc' && args[1] === 'local' && args[2] === 'bridge') {
    scheduleReconciliation(bin, pkgRoot);
  }
  const r = spawnSync(bin, args, {
    stdio: 'inherit',
    env: { ...process.env, AGIT_INSTALL_CHANNEL: 'npm_global' },
  });
  if (r.error) {
    log.error(`failed to start ${bin}: ${r.error.message}`);
    process.exit(1);
  }
  if (!(args[0] === 'rc' && args[1] === 'local' && args[2] === 'bridge')) {
    scheduleReconciliation(bin, pkgRoot);
  }
  process.exit(r.status === null ? 1 : r.status);
}

module.exports = { run };
