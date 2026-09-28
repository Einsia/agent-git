'use strict';

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const [bin, pkgRoot, launcher] = process.argv.slice(2);
if (!bin || !pkgRoot) process.exit(0);

// A transient npx or project-local binary cannot own a persistent daemon.
// npm's global root or its verified bin symlink identifies a durable install.
const root = spawnSync('npm', ['root', '-g'], { encoding: 'utf8', timeout: 5000, windowsHide: true });
let global = false;
try {
  const installed = fs.realpathSync(pkgRoot);
  const same = other => process.platform === 'win32'
    ? installed.toLowerCase() === other.toLowerCase()
    : installed === other;
  if (!root.error && root.status === 0) {
    try {
      global = same(fs.realpathSync(path.join(root.stdout.trim(), '@einsia', 'agent-git', 'npm')));
    } catch { /* A missing current npm root may leave a still-valid installed launcher. */ }
  }
  if (!global && launcher && fs.lstatSync(launcher).isSymbolicLink()
      && path.basename(path.dirname(launcher)) === 'bin'
      && same(fs.realpathSync(path.join(path.dirname(launcher), '..', 'lib', 'node_modules', '@einsia', 'agent-git', 'npm')))
      && fs.realpathSync(launcher) === fs.realpathSync(path.join(pkgRoot, 'shim.js'))) {
    global = true;
  }
} catch {
  process.exit(0);
}
if (!global) process.exit(0);

spawnSync(bin, ['rc', 'local', 'reconcile-installed', '--allow-other-installation'], {
  stdio: 'ignore',
  timeout: 35000,
  env: { ...process.env, AGIT_INSTALL_CHANNEL: 'npm_global' },
  windowsHide: true,
});
