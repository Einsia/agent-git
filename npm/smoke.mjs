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
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync, readFileSync, chmodSync, copyFileSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { protectWindowsSmokeHome } from '../scripts/windows-smoke-home.mjs'

const require = createRequire(import.meta.url)
const platform = require('./lib/platform.js')

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
// release / debug: whichever is newer wins — otherwise a stale release build makes smoke
// assert against an outdated version
const { statSync } = await import('node:fs')
const bin = process.env.AGIT_NPM_SMOKE_BINARY || ['release', 'debug']
  .map((p) => join(root, 'target', p, platform.binaryName()))
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
protectWindowsSmokeHome(work)
mkdirSync(home, { recursive: true })
const env = { ...process.env, HOME: home, USERPROFILE: home, AGIT_HOME: join(home, '.agit'),
  CODEX_HOME: join(home, '.codex'), CLAUDE_CONFIG_DIR: join(home, '.claude'),
  XDG_DATA_HOME: join(home, '.local', 'share'), XDG_CONFIG_HOME: join(home, '.config'),
  AGIT_HUB_URL: 'http://127.0.0.1:9', npm_config_yes: 'true' }
for (const key of ['AGIT_TELEMETRY_DISABLED', 'DO_NOT_TRACK', 'AGIT_TELEMETRY_DEFER', 'AGIT_SKIP_SETUP', 'AGIT_SESSION', 'AGIT_TELEMETRY_HOST', 'AGIT_TELEMETRY_KEY']) delete env[key]
if (process.platform === 'win32') {
  delete env.HOME
  delete env.AGIT_HOME
}
let failed = 0
const check = (label, ok, detail = '') => {
  console.log(`${ok ? '✓' : '✗ FAIL'} ${label}${ok ? '' : ` :: ${detail}`}`)
  if (!ok) failed++
}

// Layout: node_modules/@einsia/agent-git (the installed form) + the host's platform package +
// the install wrapper
mkdirSync(join(nm, `agent-git-${key}`, 'bin'), { recursive: true })
copyFileSync(bin, join(nm, `agent-git-${key}`, 'bin', platform.binaryName()))
chmodSync(join(nm, `agent-git-${key}`, 'bin', platform.binaryName()), 0o755)

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

const version = process.env.AGIT_NPM_SMOKE_VERSION || readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/\[package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1]

// 1. main package shim forwarding
{
  const r = spawnSync('node', [mainShim, '--version'], { encoding: 'utf8', env })
  check('shim resolves the platform binary and forwards --version', r.status === 0 && (r.stdout || '').trim() === `agit ${version}`, `${r.status} ${r.stdout} ${r.stderr}`)
}
{
  const r = spawnSync('node', [mainShim, 'definitely-not-a-command'], { encoding: 'utf8', env })
  check('shim forwards a failing exit code', r.status === 2 || r.status === 1, `exit ${r.status}`)
}

// 2. sandboxed install through the npx wrapper
{
  const preferences = join(home, '.agit', 'telemetry', 'preferences.json')
  const postinstall = spawnSync('node', [join(mainPkg, 'npm', 'postinstall.js')], { encoding: 'utf8', env: { ...env, npm_command: 'exec' }, cwd: home })
  check('dependency postinstall defers usage-statistics onboarding', postinstall.status === 0 && !existsSync(preferences), postinstall.stderr)
  const acquisitionId = 'f2ec57cb-12f0-4387-bd56-7739bde158bf'
  const r = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs'), '--acquisition-id', acquisitionId, '--campaign-url', 'https://example.test/?utm_source=docs&utm_campaign=launch&utm_creative_format=video&gclid=click&token=private-campaign-canary'], {
    encoding: 'utf8',
    env: { ...env, AGIT_TELEMETRY_HOST: 'http://127.0.0.1:9', AGIT_TELEMETRY_KEY: 'synthetic' },
    cwd: home,
  })
  const installed = join(home, '.local', 'bin', platform.binaryName())
  check(`npx wrapper installs ${platform.binaryName()} to the user bin directory`, r.status === 0 && existsSync(installed), `${r.status} ${r.stderr}`)
  if (existsSync(installed)) {
    const v = spawnSync(installed, ['--version'], { encoding: 'utf8', env })
    check('installed binary runs', v.status === 0, `${v.status} ${v.stderr}`)
    // at least one of what setup persists (the skill, the AGENTS.md marker block) must be there
    check('agit setup ran (skills marker exists)',
      existsSync(join(home, '.claude', 'skills', 'agit', 'SKILL.md')) ||
      existsSync(join(home, '.claude', 'agents.md')) ||
      existsSync(join(home, 'AGENTS.md')))
    check('setup persists usage-statistics preferences', existsSync(preferences), r.stderr)
    if (existsSync(preferences)) {
      const saved = JSON.parse(readFileSync(preferences, 'utf8'))
      check('verified install retains the browser acquisition key', saved.install_reported === true && saved.acquisition_id === acquisitionId)
      check('installer retains URL campaign context locally', saved.campaign_first?.parameters.utm_creative_format?.[0] === 'video' && saved.campaign_latest?.parameters.gclid?.[0] === 'click' && !JSON.stringify(saved).includes('private-campaign-canary'))
      check('npm yes enables statistics after a visible notice', saved.preference === 'enabled' && saved.decision_source === 'create_agit_yes' && r.stderr.includes('agit telemetry disable'), r.stderr)
      const stages = JSON.parse(readFileSync(join(home, '.agit', 'telemetry', 'queue.json'), 'utf8')).entries
        .map(entry => entry.event).filter(event => event.event === 'cli_install_stage')
      check('visible installer stages cover copy, verification, setup and completion',
        stages.map(event => event.properties.stage).join(',') === 'started,binary_copy,verification,setup,finished')
      check('installer stages share one attempt and retain acquisition without raw campaign values',
        new Set(stages.map(event => event.properties.attempt_id)).size === 1 &&
        stages.every(event => event.properties.acquisition_id === acquisitionId && event.properties.elapsed_ms >= 0) &&
        !JSON.stringify(stages).includes('private-campaign-canary'))
      spawnSync(installed, ['telemetry', 'disable'], { env, encoding: 'utf8' })
      const again = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs')], { env, encoding: 'utf8', cwd: home })
      check('reinstallation with npm yes preserves an opt-out', again.status === 0 && JSON.parse(readFileSync(preferences, 'utf8')).preference === 'disabled', again.stderr)
    }
  }
}

// A filesystem failure is observable without claiming that a binary was installed.
{
  const failedHome = join(work, 'failed-copy-home')
  mkdirSync(failedHome)
  writeFileSync(join(failedHome, '.local'), 'block directory creation')
  const failedEnv = { ...env, HOME: failedHome, USERPROFILE: failedHome, AGIT_HOME: join(failedHome, '.agit'),
    AGIT_TELEMETRY_HOST: 'http://127.0.0.1:9', AGIT_TELEMETRY_KEY: 'synthetic' }
  const result = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs')], { env: failedEnv, encoding: 'utf8', cwd: failedHome })
  const queue = join(failedHome, '.agit', 'telemetry', 'queue.json')
  const events = existsSync(queue) ? JSON.parse(readFileSync(queue, 'utf8')).entries.map(entry => entry.event) : []
  check('failed durable copy emits a classified failure without an installation receipt', result.status === 1 &&
    events.some(event => event.event === 'cli_install_stage' && event.properties.stage === 'binary_copy' && event.properties.outcome === 'error') &&
    events.some(event => event.event === 'cli_install_stage' && event.properties.stage === 'finished' && event.properties.error_category === 'filesystem') &&
    !events.some(event => event.event === 'cli_install_succeeded'), result.stderr)
}

// npm controls lifecycle visibility; running postinstall directly cannot exercise that boundary.
if (process.platform !== 'win32') {
  for (const foreground of [false, true]) {
    const lifecycleHome = join(work, foreground ? 'foreground-home' : 'hidden-home')
    const consumer = join(lifecycleHome, 'consumer')
    const fixture = join(lifecycleHome, 'fixture')
    mkdirSync(consumer, { recursive: true })
    mkdirSync(fixture, { recursive: true })
    writeFileSync(join(consumer, 'package.json'), JSON.stringify({ name: 'smoke-consumer', version: '1.0.0', private: true }))
    writeFileSync(join(fixture, 'package.json'), JSON.stringify({ name: 'verified-install-fixture', version: '1.0.0', scripts: { postinstall: 'node postinstall.cjs' } }))
    writeFileSync(join(fixture, 'postinstall.cjs'), `require(${JSON.stringify(join(mainPkg, 'npm', 'postinstall.js'))})`)
    const lifecycleEnv = { ...env, HOME: lifecycleHome, USERPROFILE: lifecycleHome,
      AGIT_HOME: join(lifecycleHome, '.agit'), AGIT_SKIP_SETUP: '1',
      AGIT_TELEMETRY_HOST: 'http://127.0.0.1:9', AGIT_TELEMETRY_KEY: 'synthetic',
      npm_config_cache: join(work, 'npm-cache'), npm_config_userconfig: join(work, 'empty-npmrc') }
    delete lifecycleEnv.npm_config_foreground_scripts
    const result = spawnSync('npm', ['install', '--offline', '--no-audit', '--no-fund', '--install-links', '--ignore-scripts=false', `--foreground-scripts=${foreground}`, fixture], {
      encoding: 'utf8', env: lifecycleEnv, cwd: consumer, timeout: 30000,
    })
    check(`real npm lifecycle succeeds (foreground=${foreground})`, result.status === 0, `${result.status} ${result.stderr}`)
    const dir = join(lifecycleHome, '.agit', 'telemetry')
    const prefs = join(dir, 'preferences.json')
    if (foreground) {
      check('foreground npm discloses before enabling statistics', existsSync(prefs) && (result.stdout + result.stderr).includes('agit telemetry disable'), result.stdout + result.stderr)
      check('foreground npm records a verified install without setup or registration', existsSync(prefs) && JSON.parse(readFileSync(prefs, 'utf8')).install_reported === true)
    } else {
      check('hidden npm output cannot enable statistics or allocate an installation ID', !existsSync(prefs) && !existsSync(join(dir, 'queue.json')) && !(result.stdout + result.stderr).includes('agit telemetry disable'))
      const pending = join(dir, 'pending-install.json')
      check('hidden npm retains the verified installation fact', existsSync(pending))
      if (existsSync(pending)) {
        const fact = JSON.parse(readFileSync(pending, 'utf8'))
        const consent = spawnSync(bin, ['telemetry', 'enable'], { encoding: 'utf8', env: lifecycleEnv, cwd: consumer })
        const queuePath = join(dir, 'queue.json')
        const receipt = existsSync(queuePath) && JSON.parse(readFileSync(queuePath, 'utf8')).entries.find(entry => entry.event.event === 'cli_install_succeeded')?.event
        check('visible consent records the original installation without registration', consent.status === 0 && consent.stderr.includes('agit telemetry disable') && receipt?.timestamp === fact.verified_at, consent.stderr)
      }
    }
  }
}

// 3. source checkout: postinstall must skip itself on this path
{
  const r = spawnSync('node', [join(root, 'npm', 'postinstall.js')], { encoding: 'utf8', env })
  check('postinstall in a source checkout stays silent', r.status === 0, `${r.status} ${r.stderr}`)
}

rmSync(work, { recursive: true, force: true })
console.log(failed === 0 ? 'smoke ok' : `smoke FAILED (${failed})`)
process.exit(failed === 0 ? 0 : 1)
