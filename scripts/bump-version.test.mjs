#!/usr/bin/env node
// Regression for `bump-version.mjs`: a version that has no CHANGELOG.md section is refused before
// any file is written, so a failed bump never leaves the manifests half-moved.
//
//   node --test scripts/bump-version.test.mjs
import { strict as assert } from 'node:assert'
import { spawnSync } from 'node:child_process'
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const VERSIONED = [
  'Cargo.toml',
  'CHANGELOG.md',
  'package.json',
  'npm/create-agit/package.json',
  'npm/platforms/linux-x64/package.json',
  'npm/platforms/linux-arm64/package.json',
  'npm/platforms/darwin-x64/package.json',
  'npm/platforms/darwin-arm64/package.json',
]

function snapshot(dir) {
  const walk = (d) =>
    readdirSync(d).flatMap((name) => {
      const p = join(d, name)
      return statSync(p).isDirectory() ? walk(p) : [p]
    })
  return Object.fromEntries(walk(dir).map((p) => [p.slice(dir.length), readFileSync(p, 'utf8')]))
}

test('a version without a changelog section changes no file', () => {
  const dir = mkdtempSync(join(tmpdir(), 'bump-'))
  try {
    for (const rel of [...VERSIONED, 'scripts/bump-version.mjs', 'scripts/release-notes.mjs', 'scripts/check-version.js']) {
      cpSync(join(root, rel), join(dir, rel))
    }
    const before = snapshot(dir)
    const run = spawnSync(process.execPath, [join(dir, 'scripts/bump-version.mjs'), '99.99.99'], {
      cwd: dir,
      encoding: 'utf8',
    })
    assert.equal(run.status, 1, run.stdout + run.stderr)
    assert.match(run.stderr, /no "## \[99\.99\.99\]" section/)
    assert.match(run.stderr, /nothing was changed/)
    assert.deepEqual(snapshot(dir), before)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
