#!/usr/bin/env node
import assert from 'node:assert/strict'
import { readFileSync, mkdtempSync, mkdirSync, rmSync, existsSync, writeFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { delimiter, join, resolve } from 'node:path'
import { protectWindowsSmokeHome } from './windows-smoke-home.mjs'

const binary = resolve(process.argv[2])
const bytes = readFileSync(binary)
assert.equal(bytes.subarray(0, 2).toString('ascii'), 'MZ', 'Windows executable needs a DOS header')
assert.ok(bytes.length >= 64, 'DOS header is truncated')
const pe = bytes.readUInt32LE(60)
assert.ok(pe <= bytes.length - 6, 'PE header is outside the executable')
assert.equal(bytes.readUInt32LE(pe), 0x00004550, 'Windows executable needs a PE signature')
assert.equal(bytes.readUInt16LE(pe + 4), 0x8664, 'Windows artifact must target x64')
assert.equal(process.platform, 'win32', 'Windows smoke tests require a native Windows host')
const home = mkdtempSync(join(tmpdir(), 'agit-windows-smoke-'))
try {
  protectWindowsSmokeHome(home)
  // Setup writes for the runtimes installed here; Claude Code has run in this home.
  mkdirSync(join(home, '.claude'), { recursive: true })
  const env = { ...process.env, USERPROFILE: home, CI: '1' }
  for (const key of Object.keys(env)) {
    if (['HOME', 'AGIT_HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME'].includes(key.toUpperCase())) delete env[key]
  }
  // A stand-in `codex` comes first on PATH. Setup asks an installed Codex about its features and
  // hooks, and a real one can leave its shared background runtime working in this home after agit
  // returns, which the cleanup below then can't remove. The stand-in answers and exits, so agit's
  // own Codex path still runs.
  const fixtureBin = join(home, 'fixture-bin')
  mkdirSync(fixtureBin, { recursive: true })
  writeFileSync(join(fixtureBin, 'codex.cmd'), '@echo %*>> "%~dp0codex-calls.txt"\r\n@if "%1 %2"=="features list" echo hooks stable true\r\n@exit /b 0\r\n')
  const pathKey = Object.keys(env).find(key => key.toUpperCase() === 'PATH') ?? 'PATH'
  env[pathKey] = `${fixtureBin}${delimiter}${env[pathKey] ?? ''}`
  for (const args of [['--version'], ['--help'], ['config', 'hub.url'], ['setup'], ['-y', 'init', 'windows-smoke', '--no-bind']]) {
    const result = spawnSync(binary, args, {
      encoding: 'utf8',
      env,
      cwd: home,
    })
    assert.equal(result.status, 0, `${args.join(' ')} failed: ${result.error || result.stderr}`)
    assert.ok(result.stdout.trim(), `${args.join(' ')} produced no output`)
    console.log(`${args.join(' ')}: ok`)
  }
  // The stand-in only keeps the host's runtime out if setup reaches it rather than another codex.
  assert.match(readFileSync(join(fixtureBin, 'codex-calls.txt'), 'utf8'), /^features list\r?$/m, 'setup must query the stand-in Codex')
  assert.ok(existsSync(join(home, '.agit')), 'startup must create the store under USERPROFILE')
  assert.ok(existsSync(join(home, '.claude', 'skills', 'agit', 'SKILL.md')), 'setup must install the skill under USERPROFILE')
  const initialized = spawnSync('git', ['-C', join(home, '.agit', 'repos', 'local', 'windows-smoke'), 'show-ref', '--verify', 'refs/heads/main'], { encoding: 'utf8', env })
  assert.equal(initialized.status, 0, `init must create a real main branch: ${initialized.stderr}`)
  const status = spawnSync(binary, ['rc', 'status'], { encoding: 'utf8', env, cwd: home, timeout: 5000 })
  assert.equal(status.error, undefined, 'RC status without a daemon must return promptly')
  assert.notEqual(status.status, 0, 'an absent daemon must not report success')
  assert.match(status.stderr, /no daemon is running/, 'native RC must distinguish absence from unsupported operation')
  assert.ok(!existsSync(join(home, '.agit', 'rc', 'identity.json')), 'status must not pair or create an identity')
} finally {
  rmSync(home, { recursive: true, force: true })
}
