#!/usr/bin/env node
/**
 * Local smoke test for the npm distribution: verifies every link in "it really runs once
 * installed" without touching the registry.
 *
 *   node npm/smoke.mjs
 *
 * The layers it verifies:
 *   - the main package's shim resolves the platform binary out of the optional-dep layout and
 *     forwards the exit code;
 *   - the npx wrapper (create-agit/bin.mjs) lands the binary in a sandboxed $HOME/.local/bin
 *     and gets `agit setup` through;
 *   - postinstall's self-check does not fire on a path outside node_modules (a source
 *     checkout).
 *
 * The layout is assembled in the real shape of the published tarball: the main package root
 * holds package.json plus npm/ (the files manifest keeps the npm/ prefix), so a cross-package
 * subpath require like `@einsia/agent-git/npm/lib/run` resolves here the way it does in a real
 * install — a flattened layout does not catch a broken subpath. The platform package key is
 * derived from the host, so both linux and darwin dev machines reach the real branch.
 */

import { spawnSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync, readFileSync, chmodSync, copyFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)
const platform = require('./lib/platform.js')

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
// release / debug: whichever is newer wins — otherwise a stale release build makes smoke
// assert against an outdated version
const { statSync } = await import('node:fs')
const bin = ['release', 'debug']
  .map((p) => join(root, 'target', p, 'agit'))
  .filter(existsSync)
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0]
if (!bin) {
  console.error('run `cargo build` (or --release) first — smoke uses target/{release,debug}/agit')
  process.exit(1)
}

const key = platform.packageKey()
if (!key) {
  console.error(`no platform package key for ${process.platform}/${process.arch} — smoke needs a supported host`)
  process.exit(1)
}

const work = mkdtempSync(join(tmpdir(), 'agit-npm-smoke-'))
const nm = join(work, 'node_modules', '@einsia')
const home = join(work, 'home')
let failed = 0
const check = (label, ok, detail = '') => {
  console.log(`${ok ? '✓' : '✗ FAIL'} ${label}${ok ? '' : ` :: ${detail}`}`)
  if (!ok) failed++
}

// Layout: node_modules/@einsia/agent-git (the installed form) + the host's platform package +
// the install wrapper
mkdirSync(join(nm, `agent-git-${key}`, 'bin'), { recursive: true })
copyFileSync(bin, join(nm, `agent-git-${key}`, 'bin', 'agit'))
chmodSync(join(nm, `agent-git-${key}`, 'bin', 'agit'), 0o755)

const mainPkg = join(nm, 'agent-git')
mkdirSync(mainPkg, { recursive: true })
copyFileSync(join(root, 'package.json'), join(mainPkg, 'package.json'))
cpSync(join(root, 'npm'), join(mainPkg, 'npm'), {
  recursive: true,
  // only what the published tarball also carries: the subpackage directories and the publish
  // scripts are not in the main package
  filter: (s) => {
    const rel = relative(join(root, 'npm'), s)
    return (
      !rel.startsWith('platforms') &&
      !rel.startsWith('create-agit') &&
      rel !== 'publish.mjs' &&
      rel !== 'smoke.mjs'
    )
  },
})
const mainShim = join(mainPkg, 'npm', 'shim.js')

mkdirSync(join(work, 'node_modules', 'create-agit'), { recursive: true })
cpSync(join(root, 'npm', 'create-agit'), join(work, 'node_modules', 'create-agit'), { recursive: true })

const version = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/\[package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1]

// 1. main package shim forwarding
{
  const r = spawnSync('node', [mainShim, '--version'], { encoding: 'utf8' })
  check('shim resolves the platform binary and forwards --version', r.status === 0 && (r.stdout || '').trim() === `agit ${version}`, `${r.status} ${r.stdout} ${r.stderr}`)
}
{
  const r = spawnSync('node', [mainShim, 'definitely-not-a-command'], { encoding: 'utf8' })
  check('shim forwards a failing exit code', r.status === 2 || r.status === 1, `exit ${r.status}`)
}

// 2. sandboxed install through the npx wrapper
{
  mkdirSync(home, { recursive: true })
  const r = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs')], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home },
  })
  const installed = join(home, '.local', 'bin', 'agit')
  check('npx wrapper installs to $HOME/.local/bin/agit', r.status === 0 && existsSync(installed), `${r.status} ${r.stderr}`)
  if (existsSync(installed)) {
    const v = spawnSync(installed, ['--version'], { encoding: 'utf8' })
    check('installed binary runs', v.status === 0, `${v.status} ${v.stderr}`)
    // at least one of what setup persists (the skill, the AGENTS.md marker block) must be there
    check('agit setup ran (skills marker exists)',
      existsSync(join(home, '.claude', 'skills', 'agit', 'SKILL.md')) ||
      existsSync(join(home, '.claude', 'agents.md')) ||
      existsSync(join(home, 'AGENTS.md')))
  }
}

// 3. source checkout: postinstall must skip itself on this path
{
  const r = spawnSync('node', [join(root, 'npm', 'postinstall.js')], { encoding: 'utf8' })
  check('postinstall in a source checkout stays silent', r.status === 0, `${r.status} ${r.stderr}`)
}

rmSync(work, { recursive: true, force: true })
console.log(failed === 0 ? 'smoke ok' : `smoke FAILED (${failed})`)
process.exit(failed === 0 ? 0 : 1)
