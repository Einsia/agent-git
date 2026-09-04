#!/usr/bin/env node
/**
 * Write the version number into every place it lands, in one pass.
 *
 *   node scripts/bump-version.mjs 0.10.0
 *
 * The one source for the version is the [package] version in Cargo.toml; on the npm side the
 * main package, the installer wrapper and the four platform packages must carry the same
 * value, and so must the pins on the main package and the platform packages in
 * optionalDependencies / dependencies — one digit apart and whoever installs gets a 404, or a
 * binary of a different version. `scripts/check-version.js` is the backstop in CI and in
 * prepack; this script is what changes them all at once, and brings this package's own entry
 * in Cargo.lock along with it.
 *
 * The release flow from there is in README's Release section: commit →
 * `git tag agit-vX.Y.Z` → push the tag — release.yml builds the binaries, and on success the
 * npm publish workflow takes over and publishes the whole family.
 */

import { spawnSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const version = process.argv[2]

// The semantic-version shape; a pre-release suffix (0.10.0-rc.1) is valid, and the release
// flow marks it pre-release so it does not take Latest.
if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error('usage: node scripts/bump-version.mjs <X.Y.Z[-pre]>')
  process.exit(1)
}

// The release notes come before the version. A bump with no section to ship stops here, with
// every file as it was, rather than part-way through the manifests with the gate below red.
{
  const notes = spawnSync(process.execPath, [join(root, 'scripts', 'release-notes.mjs'), version], {
    encoding: 'utf8',
  })
  if (notes.status !== 0) {
    process.stderr.write(notes.stderr || '')
    console.error(`write the "## [${version}]" section of CHANGELOG.md first; nothing was changed.`)
    process.exit(1)
  }
}

// Only the version line that comes first in the [package] section changes; the
// section-boundary test is the same one check-version.js uses: every dependency
// version = "..." sits in a later section, out of reach.
{
  const path = join(root, 'Cargo.toml')
  const lines = readFileSync(path, 'utf8').split('\n')
  let inPackage = false
  let done = false
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim()
    if (line.startsWith('[')) {
      inPackage = line === '[package]'
      continue
    }
    if (!inPackage || done) continue
    if (/^version\s*=\s*"/.test(line)) {
      lines[i] = lines[i].replace(/version(\s*=\s*)"[^"]+"/, `version$1"${version}"`)
      done = true
    }
  }
  if (!done) {
    console.error('no version in the [package] section of Cargo.toml')
    process.exit(1)
  }
  writeFileSync(path, lines.join('\n'))
  console.log(`Cargo.toml → ${version}`)
}

// The whole npm family: version itself, plus the pins on the main package and the platform
// packages.
const manifests = [
  'package.json',
  'npm/create-agit/package.json',
  'npm/platforms/linux-x64/package.json',
  'npm/platforms/linux-arm64/package.json',
  'npm/platforms/darwin-x64/package.json',
  'npm/platforms/darwin-arm64/package.json',
]
for (const rel of manifests) {
  const path = join(root, rel)
  const pkg = JSON.parse(readFileSync(path, 'utf8'))
  pkg.version = version
  for (const dep of Object.keys(pkg.optionalDependencies || {})) {
    pkg.optionalDependencies[dep] = version
  }
  for (const dep of Object.keys(pkg.dependencies || {})) {
    if (dep === '@einsia/agent-git') pkg.dependencies[dep] = version
  }
  writeFileSync(path, JSON.stringify(pkg, null, 2) + '\n')
  console.log(`${rel} → ${version}`)
}

// Cargo.lock records this package's own version; out of sync, the next --locked build refuses
// outright. `--workspace` touches only workspace-member entries and upgrades no dependency.
{
  const r = spawnSync('cargo', ['update', '--workspace'], { cwd: root, stdio: 'inherit' })
  if (r.error || r.status !== 0) {
    console.error('cargo update --workspace did not run — run it by hand so Cargo.lock keeps up.')
    process.exit(1)
  }
}

const check = spawnSync('node', [join(root, 'scripts', 'check-version.js')], { cwd: root, stdio: 'inherit' })
if (check.status !== 0) process.exit(check.status ?? 1)

console.log(`
version ${version} staged (the CHANGELOG.md section for it is the Release body). Next:
  git add -A && git commit
  git tag agit-v${version} && git push origin agit-v${version}
  # release.yml builds the binaries; on success the "npm publish" workflow takes over and publishes the npm family.`)
