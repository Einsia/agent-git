'use strict';

const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const RETRY_MS = 5 * 60 * 1000;

// An npm policy may suppress postinstall. The next CLI use retries the guarded
// daemon handoff without changing the command's output or exit status.
function scheduleReconciliation(bin, pkgRoot) {
  if (process.env.AGIT_BINARY || process.env.AGIT_INTERNAL_UPDATE_RESTART) return;
  const home = process.env.AGIT_HOME?.trim() || path.join(os.homedir(), '.agit');
  const dir = path.join(home, 'desktop-rc');
  let fingerprint;
  try {
    const daemon = fs.statSync(path.join(dir, 'agitd.pid'));
    const installed = fs.statSync(bin);
    fingerprint = `${path.resolve(bin)}:${installed.size}:${installed.mtimeMs}:${daemon.size}:${daemon.mtimeMs}`;
    const stamp = path.join(dir, 'npm-reconcile.json');
    try {
      const previous = JSON.parse(fs.readFileSync(stamp, 'utf8'));
      if (previous.fingerprint === fingerprint && Date.now() - previous.at < RETRY_MS) return;
    } catch { /* An absent or invalid retry receipt cannot suppress reconciliation. */ }
    const temporary = `${stamp}.${process.pid}.${Math.random().toString(16).slice(2)}`;
    try {
      fs.writeFileSync(temporary, JSON.stringify({ fingerprint, at: Date.now() }), { mode: 0o600, flag: 'wx' });
      fs.renameSync(temporary, stamp);
    } catch {
      try { fs.unlinkSync(temporary); } catch { /* The best-effort receipt is optional. */ }
    }
  } catch {
    return; // No running daemon is indicated in this home.
  }

  try {
    const child = spawn(process.execPath, [path.join(__dirname, 'reconcile-worker.js'), bin, pkgRoot, process.argv[1] || ''], {
      detached: true,
      stdio: 'ignore',
      env: { ...process.env, AGIT_TELEMETRY_DEFER: '1', AGIT_INTERNAL_UPDATE_RESTART: '1' },
      windowsHide: true,
    });
    child.on('error', () => {});
    child.unref();
  } catch {
    // Reconciliation never changes the forwarded command's result.
  }
}

module.exports = { scheduleReconciliation };
