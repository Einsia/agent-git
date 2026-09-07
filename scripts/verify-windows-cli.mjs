#!/usr/bin/env node
import assert from 'node:assert/strict'
import { readFileSync, mkdtempSync, rmSync, existsSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

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
  const protect = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
    $ErrorActionPreference = 'Stop'
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
    [System.IO.Directory]::SetAccessControl($env:AGIT_WINDOWS_SMOKE_HOME, $acl)
  `], { encoding: 'utf8', env: { ...process.env, AGIT_WINDOWS_SMOKE_HOME: home }, timeout: 10000 })
  assert.equal(protect.status, 0, `owned Windows smoke home needs a private ACL: ${protect.error || protect.stderr}`)
  const env = { ...process.env, USERPROFILE: home, CI: '1' }
  for (const key of Object.keys(env)) {
    if (['HOME', 'AGIT_HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME'].includes(key.toUpperCase())) delete env[key]
  }
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
