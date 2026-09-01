'use strict';

const fs = require('fs');
const path = require('path');
const platform = require('./platform');

// Where the real binary is. Distribution is a platform subpackage
// (@einsia/agent-git-<key>; npm installs only the one matching os/cpu), so the
// default meaning is "find that package in the dependency tree". The resolution
// order is deliberately fixed:
//
//   1. An explicit AGIT_BINARY override — the user is pointing the way and gets
//      the last word.
//   2. This package's bin/<agit> — placed by hand outside the publish flow (a
//      build a volunteer is testing, for example).
//   3. The platform subpackage's package/bin/<agit> — the normal path.
//
// A require that resolves nothing means npm skipped the optional dep for this
// platform (an unsupported system).
function resolveBinary(pkgRoot) {
  const override = process.env.AGIT_BINARY;
  if (override) {
    if (!fs.existsSync(override)) {
      throw new Error(`AGIT_BINARY points at "${override}", but there is no file there.`);
    }
    return override;
  }

  const local = path.join(pkgRoot, 'bin', platform.binaryName());
  if (fs.existsSync(local)) return local;

  const key = platform.packageKey();
  if (!key) return null;
  const name = path.join(platform.packageName(key), 'bin', platform.binaryName()).replace(/\\/g, '/');
  // Resolve by walking node_modules up from the package root; that matches
  // where npm installs the optional dep.
  try {
    return require.resolve(name, { paths: [pkgRoot] });
  } catch {
    return null;
  }
}

module.exports = { resolveBinary };
