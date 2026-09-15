'use strict';

// postinstall: the binary lands with the optional dep, so there is nothing to download. It
// does two things in passing, each idempotent and neither able to block the install when it
// fails:
//
// 1. Self-check that the binary really runs (an architecture mismatch, Rosetta and the like
//    surface here rather than the first time the user types agit).
// 2. Run `agit setup` — the product promise of "installed by default at download time":
//    skills, hooks, MCP and AGENTS.md, all in one go. AGIT_SKIP_SETUP=1 turns it off
//    (CI / hosted environments).
//
// npm displays postinstall output badly, so stay quiet: speak only when something is wrong.

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');
const { resolveBinary } = require('./lib/resolve');
const log = require('./lib/log');

function truthy(v) {
  return v != null && v !== '' && v !== '0' && String(v).toLowerCase() !== 'false';
}

function main() {
  // Whoever runs `npm install` inside the source repository is not a user: a Cargo.toml next
  // door and no node_modules in its own path = the contributor path, and contributors go through
  // ./setup.sh. AGIT_FORCE_INSTALL=1 bypasses this (CI smoke test).
  const pkgRoot = path.join(__dirname);
  const inNodeModules = pkgRoot.split(path.sep).includes('node_modules');
  if (!truthy(process.env.AGIT_FORCE_INSTALL) &&
      !inNodeModules && fs.existsSync(path.join(pkgRoot, '..', 'Cargo.toml'))) {
    return;
  }

  let bin;
  try {
    bin = resolveBinary(pkgRoot);
  } catch (e) {
    log.warn(e.message);
    return;
  }
  if (!bin) return; // unsupported platform: the shim gives full instructions on the next command.

  const env = { ...process.env, AGIT_TELEMETRY_DEFER: '1', AGIT_INSTALL_CHANNEL: 'npm_global' };
  const check = spawnSync(bin, ['--version'], { encoding: 'utf8', env });
  if (check.error || check.status !== 0) {
    log.warn(`installed binary does not run: ${bin}`);
    log.warn((check.stderr || check.error?.message || `exit ${check.status}`).trim());
    return;
  }

  // npx create-agit owns its durable-copy receipt and passes the browser acquisition key.
  if (process.env.npm_command !== 'exec') {
    const receiptArgs = ['--internal-install-completed'];
    if (!truthy(process.env.npm_config_foreground_scripts)) receiptArgs.push('--defer-notice');
    spawnSync(bin, receiptArgs, {
      stdio: ['ignore', 'inherit', 'inherit'],
      env: { ...process.env, AGIT_INSTALL_CHANNEL: 'npm_global' },
    });
  }
  if (truthy(process.env.AGIT_SKIP_SETUP)) return;
  const setup = spawnSync(bin, ['setup'], { stdio: 'inherit', env });
  if (setup.status !== 0) {
    log.warn('`agit setup` did not fully succeed — re-run it any time; the CLI itself is installed.');
  }
}

main();
