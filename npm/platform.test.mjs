import test from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { mkdtempSync, mkdirSync, cpSync, writeFileSync, readFileSync, rmSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { spawnSync, execFileSync } from 'node:child_process'

const require = createRequire(import.meta.url)
const platform = require('./lib/platform.js')
const root = join(dirname(fileURLToPath(import.meta.url)), '..')

test('Windows x64 resolves the executable package and its matching release target', () => {
  const key = platform.packageKey('win32', 'x64')
  assert.equal(key, 'win32-x64')
  assert.equal(platform.TRIPLES[key], 'x86_64-pc-windows-msvc')
  assert.equal(platform.binaryName('win32'), 'agit.exe')
  assert.equal(platform.packageKey('win32', 'arm64'), null)
  const main = require('../package.json')
  const pkg = require('./platforms/win32-x64/package.json')
  assert.equal(main.optionalDependencies[pkg.name], pkg.version)
  assert.deepEqual(pkg.os, ['win32'])
  assert.deepEqual(pkg.cpu, ['x64'])
  assert.equal(pkg.bin.agit, 'bin/agit.exe')
})

for (const [machine, accepted] of [[0x8664, true], [0x14c, false], [0xaa64, false]]) {
  test(`publisher validates the Windows PE machine ${machine.toString(16)}`, () => {
    const dir = mkdtempSync(join(tmpdir(), 'agit-windows-publish-'))
    try {
      cpSync(join(root, 'npm'), join(dir, 'npm'), { recursive: true })
      cpSync(join(root, 'package.json'), join(dir, 'package.json'))
      cpSync(join(root, 'Cargo.toml'), join(dir, 'Cargo.toml'))
      const version = JSON.parse(readFileSync(join(dir, 'package.json'))).version
      const input = join(dir, 'input')
      const artifacts = join(dir, 'artifacts')
      mkdirSync(input)
      mkdirSync(artifacts)
      const bytes = Buffer.alloc(128)
      bytes.write('MZ')
      bytes.writeUInt32LE(64, 60)
      bytes.writeUInt32LE(0x00004550, 64)
      bytes.writeUInt16LE(machine, 68)
      writeFileSync(join(input, 'agit.exe'), bytes, { mode: 0o755 })
      execFileSync('tar', ['-C', input, '-czf', join(artifacts, `agit-${version}-x86_64-pc-windows-msvc.tar.gz`), 'agit.exe'])
      const result = spawnSync(process.execPath, [join(dir, 'npm/publish.mjs'), artifacts, '--dry-run', '--allow-partial', '--only', 'platforms'], { encoding: 'utf8' })
      assert.equal(result.status === 0, accepted, result.stdout + result.stderr)
      if (accepted) {
        assert.match(result.stdout, /staged win32-x64: \(cross, PE x64 ok\)/)
        assert.match(result.stdout, /publish @einsia\/agent-git-win32-x64@/)
        assert.ok(existsSync(join(dir, 'npm/platforms/win32-x64/bin/agit.exe')))
      } else {
        assert.match(result.stderr, /corrupt staging/)
      }
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })
}
