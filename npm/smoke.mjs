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
import { createHash } from 'node:crypto'
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync, readFileSync, readdirSync, chmodSync, copyFileSync, symlinkSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { delimiter, dirname, join, relative } from 'node:path'
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
// The installer asks its registry for the newest create-agit; a registry that refuses keeps every
// run off the network and on the same freshness path.
const env = { ...process.env, HOME: home, USERPROFILE: home, AGIT_HOME: join(home, '.agit'),
  CODEX_HOME: join(home, '.codex'), CLAUDE_CONFIG_DIR: join(home, '.claude'),
  XDG_DATA_HOME: join(home, '.local', 'share'), XDG_CONFIG_HOME: join(home, '.config'),
  AGIT_HUB_URL: 'http://127.0.0.1:9', npm_config_registry: 'http://127.0.0.1:9/', npm_config_yes: 'true' }
for (const key of ['AGIT_TELEMETRY_DISABLED', 'DO_NOT_TRACK', 'AGIT_TELEMETRY_DEFER', 'AGIT_SKIP_SETUP', 'AGIT_SESSION', 'AGIT_TELEMETRY_HOST', 'AGIT_TELEMETRY_KEY']) delete env[key]
if (process.platform === 'win32') {
  delete env.HOME
  delete env.AGIT_HOME
}
// A stand-in `codex` comes first on every step's PATH. agit asks an installed Codex about its
// features and hooks, and a real one can leave work of its own running in this run's homes after
// agit returns, which the cleanup below would then meet. The stand-in answers and exits, so agit's
// own Codex path still runs.
const fixtureBin = join(work, 'fixture-bin')
mkdirSync(fixtureBin, { recursive: true })
writeFileSync(join(fixtureBin, 'codex'), '#!/bin/sh\n[ "$1 $2" = "features list" ] && echo "hooks stable true"\nexit 0\n', { mode: 0o755 })
writeFileSync(join(fixtureBin, 'codex.cmd'), '@if "%1 %2"=="features list" echo hooks stable true\r\n@exit /b 0\r\n')
const pathKey = Object.keys(env).find(key => key.toUpperCase() === 'PATH') ?? 'PATH'
env[pathKey] = `${fixtureBin}${delimiter}${env[pathKey] ?? ''}`
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
  // Setup writes for the runtimes installed here; Claude Code has run in this home.
  mkdirSync(join(home, '.claude'), { recursive: true })
  // npx starts the installer with its cache's node_modules/.bin ahead of the person's PATH, and
  // that folder links agit to the shim; the person's shell selects no agit there.
  const npxBin = join(work, 'node_modules', '.bin')
  const posix = process.platform !== 'win32'
  if (posix) {
    mkdirSync(npxBin, { recursive: true })
    chmodSync(mainShim, 0o755)
    symlinkSync(mainShim, join(npxBin, 'agit'))
  }
  const r = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs'), '--acquisition-id', acquisitionId, '--campaign-url', 'https://example.test/?utm_source=docs&utm_campaign=launch&utm_creative_format=video&gclid=click&token=private-campaign-canary'], {
    encoding: 'utf8',
    env: { ...env, AGIT_TELEMETRY_HOST: 'http://127.0.0.1:9', AGIT_TELEMETRY_KEY: 'synthetic', ...(posix ? { PATH: `${npxBin}${delimiter}${env.PATH}` } : {}) },
    cwd: home,
  })
  const installed = join(home, '.local', 'bin', platform.binaryName())
  check(`npx wrapper installs ${platform.binaryName()} to the user bin directory`, r.status === 0 && existsSync(installed), `${r.status} ${r.stderr}`)
  if (posix) check("the installer does not name npx's own agit as the one the shell selects", !r.stdout.includes(npxBin), r.stdout)
  if (existsSync(installed)) {
    const v = spawnSync(installed, ['--version'], { encoding: 'utf8', env })
    check('installed binary runs', v.status === 0, `${v.status} ${v.stderr}`)
    check('agit setup ran (skills marker exists)',
      existsSync(join(home, '.claude', 'skills', 'agit', 'SKILL.md')))
    // The installer ran in the home folder; an AGENTS.md section there would reach every
    // conversation under it (CLI #198, #211).
    check('setup writes no AGENTS.md into the folder it runs in', !existsSync(join(home, 'AGENTS.md')))
    check('setup persists usage-statistics preferences', existsSync(preferences), r.stderr)
    if (existsSync(preferences)) {
      const saved = JSON.parse(readFileSync(preferences, 'utf8'))
      check('verified install retains the browser acquisition key', saved.install_reported === true && saved.acquisition_id === acquisitionId)
      check('installer retains URL campaign context locally', saved.campaign_first?.parameters.utm_creative_format?.[0] === 'video' && saved.campaign_latest?.parameters.gclid?.[0] === 'click' && !JSON.stringify(saved).includes('private-campaign-canary'))
      check('npm yes enables statistics after a visible notice', saved.preference === 'enabled' && saved.decision_source === 'create_agit_yes' && r.stderr.includes('AGIT_TELEMETRY_DISABLED'), r.stderr)
      const stages = JSON.parse(readFileSync(join(home, '.agit', 'telemetry', 'queue.json'), 'utf8')).entries
        .map(entry => entry.event).filter(event => event.event === 'cli_install_stage')
      check('visible installer stages cover copy, verification, setup and completion',
        stages.map(event => event.properties.stage).join(',') === 'started,binary_copy,verification,setup,finished')
      check('installer stages share one attempt and retain acquisition without raw campaign values',
        new Set(stages.map(event => event.properties.attempt_id)).size === 1 &&
        stages.every(event => event.properties.acquisition_id === acquisitionId && event.properties.elapsed_ms >= 0) &&
        !JSON.stringify(stages).includes('private-campaign-canary'))
      writeFileSync(preferences, JSON.stringify({ preference: 'disabled', generation: saved.generation + 1 }))
      // An agit ahead of the installed one on the person's own PATH is still named.
      const shadow = join(work, 'shadow-bin')
      if (posix) {
        mkdirSync(shadow, { recursive: true })
        symlinkSync(bin, join(shadow, 'agit'))
      }
      const again = spawnSync('node', [join(work, 'node_modules', 'create-agit', 'bin.mjs')], { env: { ...env, ...(posix ? { PATH: `${shadow}${delimiter}${env.PATH}` } : {}) }, encoding: 'utf8', cwd: home })
      check('reinstallation with npm yes preserves an opt-out', again.status === 0 && JSON.parse(readFileSync(preferences, 'utf8')).preference === 'disabled', again.stderr)
      if (posix) check('an agit ahead of the installed one on PATH is named', again.stdout.includes(`selects another agit at ${join(shadow, 'agit')}`), again.stdout)
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

// An installation whose install script did not run (npm 12 without --allow-scripts, a policy that
// skips scripts, pnpm) is installed but not set up. A command that starts or records capture,
// run by no person, says so once a day once it has done what it was asked, and the one path that
// notice writes is its record in AGIT_HOME: nothing in the runtime's folder. A command refused
// before it acts says nothing and writes no record. A read never carries the notice and leaves the
// home byte for byte as it was, telemetry aside. The first run settles what the command itself
// writes, with the notice silenced.
{
  const bare = join(work, 'not-set-up-home')
  const project = join(bare, 'project')
  mkdirSync(join(bare, '.claude'), { recursive: true })
  mkdirSync(project)
  const bareEnv = { ...env, HOME: bare, USERPROFILE: bare, AGIT_HOME: join(bare, '.agit'),
    CODEX_HOME: join(bare, '.codex'), CLAUDE_CONFIG_DIR: join(bare, '.claude'),
    XDG_DATA_HOME: join(bare, '.local', 'share'), XDG_CONFIG_HOME: join(bare, '.config') }
  const agit = (args, extra = {}) => spawnSync(bin, args, {
    encoding: 'utf8', env: { ...bareEnv, ...extra }, cwd: project, stdio: ['ignore', 'pipe', 'pipe'],
  })
  const paths = () => readdirSync(bare, { recursive: true }).map(String).sort()
  // Files are compared by digest: once a command has run Git, the home holds agit's bundled Git
  // runtime, which is too large to hold whole in one string.
  const digest = path => createHash('sha256').update(readFileSync(join(bare, path))).digest('hex')
  const contents = () => JSON.stringify(paths()
    .filter(path => !path.startsWith(join('.agit', 'telemetry')))
    .map(path => [path, statSync(join(bare, path)).isFile() ? digest(path) : null]))
  const notice = 'agit is not set up on this computer, so nothing is captured'
  const silenced = agit(['save'], { AGIT_SKIP_SETUP: '1' })
  const before = paths()
  const home = contents()
  const read = agit(['status'])
  const afterRead = contents()
  const json = agit(['--json', 'save'])
  let document = null
  try { document = JSON.parse(json.stdout) } catch {}
  const afterJson = paths()
  const refused = agit(['commit'])
  const recordAfterRefusal = existsSync(join(bare, '.agit', 'setup-notice.json'))
  const beforeFirst = paths()
  const first = agit(['save'])
  const added = paths().filter(path => !beforeFirst.includes(path))
  const second = agit(['save'])
  check('a command refused before it acts says nothing about setup and writes no record',
    refused.status !== 0 && !refused.stderr.includes(notice) && !recordAfterRefusal,
    `${refused.status} ${refused.stderr}`)
  check('an agit that was never set up says so once and writes only its record',
    !silenced.stderr.includes(notice) && first.stderr.includes(notice) && !second.stderr.includes(notice) &&
    added.length === 1 && added[0] === join('.agit', 'setup-notice.json'),
    `added ${JSON.stringify(added)} :: ${first.stderr}`)
  check('a read of an agit that was never set up says nothing and leaves its home unchanged',
    !read.stderr.includes(notice) && !read.stdout.includes(notice) && afterRead === home,
    `${read.status} ${read.stderr}`)
  check('a JSON run of an agit that was never set up keeps stdout one document and writes nothing',
    document !== null && !json.stdout.includes(notice) && !json.stderr.includes(notice) &&
    JSON.stringify(afterJson) === JSON.stringify(before),
    `${json.status} ${json.stdout} ${json.stderr}`)
}

// `agit setup --hooks` installs capture hooks and no Skill. Hooks capture adopted sessions, so a
// home set up that way is set up: a capture command says nothing about setup and writes no notice
// record, and doctor does not call it not set up. The hook files can hold secrets (Claude Code's
// `env` block, Hermes' provider keys), so once the one that set up agit no longer parses, nothing
// either holds reaches any output of save or doctor, though both are read.
{
  const hooksOnly = join(work, 'hooks-only-home')
  const project = join(hooksOnly, 'project')
  mkdirSync(join(hooksOnly, '.claude'), { recursive: true })
  mkdirSync(project)
  const hooksEnv = { ...env, HOME: hooksOnly, USERPROFILE: hooksOnly, AGIT_HOME: join(hooksOnly, '.agit'),
    CODEX_HOME: join(hooksOnly, '.codex'), CLAUDE_CONFIG_DIR: join(hooksOnly, '.claude'),
    HERMES_HOME: join(hooksOnly, '.hermes'),
    XDG_DATA_HOME: join(hooksOnly, '.local', 'share'), XDG_CONFIG_HOME: join(hooksOnly, '.config') }
  const agit = args => spawnSync(bin, args, {
    encoding: 'utf8', env: hooksEnv, cwd: project, stdio: ['ignore', 'pipe', 'pipe'],
  })
  const notice = 'agit is not set up on this computer, so nothing is captured'
  const record = join(hooksOnly, '.agit', 'setup-notice.json')
  const setup = agit(['setup', '--hooks', '--runtime', 'claude-code'])
  const skill = existsSync(join(hooksOnly, '.claude', 'skills', 'agit', 'SKILL.md'))
  const saved = [agit(['save']), agit(['save'])]
  const doctor = agit(['doctor'])
  const doctorText = doctor.stdout + doctor.stderr
  check('a home set up with hooks only is not told that nothing is captured',
    setup.status === 0 && !skill && saved.every(run => !run.stderr.includes(notice)) && !existsSync(record) &&
    !doctorText.includes('not set up') && doctorText.includes('capture hooks are installed'),
    `${setup.status} ${skill} ${saved.map(run => run.stderr).join(' :: ')} :: ${doctorText}`)
  const canary = 'agit-smoke-settings-canary-5c1e'
  writeFileSync(join(hooksOnly, '.claude', 'settings.json'), `{"env": {"SMOKE_TOKEN": "${canary}"}, "hooks": `)
  mkdirSync(join(hooksOnly, '.hermes'), { recursive: true })
  writeFileSync(join(hooksOnly, '.hermes', 'config.yaml'), `model:\n  api_key: ${canary}\nhooks: [\n`)
  const broken = [agit(['save']), agit(['doctor'])]
  check('a hook file that no longer parses leaks nothing it holds',
    broken.every(run => !run.stdout.includes(canary) && !run.stderr.includes(canary)),
    broken.map(run => `${run.status}`).join(' '))
}

// A runtime counts as here once it has run and made its folder, not because its program is on
// PATH: a home with `claude` on PATH and no Claude Code folder is not told anything, so the result
// is the same on every machine. Once the folder exists it is. `agit setup` does not use that
// test, and still installs for a runtime that is only on PATH, whose first conversation would
// otherwise go uncaptured.
if (process.platform !== 'win32') {
  const pathOnly = join(work, 'path-only-home')
  const project = join(pathOnly, 'project')
  mkdirSync(join(pathOnly, 'bin'), { recursive: true })
  mkdirSync(project)
  writeFileSync(join(pathOnly, 'bin', 'claude'), '#!/bin/sh\nexit 0\n')
  chmodSync(join(pathOnly, 'bin', 'claude'), 0o755)
  const pathOnlyEnv = { ...env, HOME: pathOnly, AGIT_HOME: join(pathOnly, '.agit'),
    CODEX_HOME: join(pathOnly, '.codex'), CLAUDE_CONFIG_DIR: join(pathOnly, '.claude'),
    XDG_DATA_HOME: join(pathOnly, '.local', 'share'), XDG_CONFIG_HOME: join(pathOnly, '.config'),
    [pathKey]: `${join(pathOnly, 'bin')}${delimiter}${env[pathKey]}` }
  const agit = (args, extra = {}) => spawnSync(bin, args, {
    encoding: 'utf8', env: { ...pathOnlyEnv, ...extra }, cwd: project, stdio: ['ignore', 'pipe', 'pipe'],
  })
  const notice = 'agit is not set up on this computer, so nothing is captured'
  const record = join(pathOnly, '.agit', 'setup-notice.json')
  agit(['save'], { AGIT_SKIP_SETUP: '1' })
  const onPath = agit(['save'])
  mkdirSync(join(pathOnly, '.claude'))
  const ran = agit(['save'])
  check('a runtime only on PATH is not reason to say agit is not set up; its folder is',
    !onPath.stderr.includes(notice) && ran.stderr.includes(notice),
    `${onPath.stderr} :: ${ran.stderr}`)
  rmSync(join(pathOnly, '.claude'), { recursive: true, force: true })
  rmSync(record, { force: true })
  const setup = agit(['setup', '--skill'])
  check('setup still installs for a runtime that is only on PATH',
    setup.status === 0 && existsSync(join(pathOnly, '.claude', 'skills', 'agit', 'SKILL.md')),
    `${setup.status} ${setup.stderr}`)
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
      npm_config_cache: join(work, 'npm-cache'), npm_config_userconfig: join(work, 'empty-npmrc'),
      npm_config_prefix: join(lifecycleHome, 'prefix') }
    delete lifecycleEnv.npm_config_foreground_scripts
    const npmInstall = (global) => spawnSync('npm', ['install', ...(global ? ['-g'] : []), '--offline', '--no-audit', '--no-fund', '--install-links', '--ignore-scripts=false', `--foreground-scripts=${foreground}`, fixture], {
      encoding: 'utf8', env: lifecycleEnv, cwd: consumer, timeout: 30000,
    })
    const dir = join(lifecycleHome, '.agit', 'telemetry')
    // A local install's binary goes away with its folder, so it records no installation; only a
    // global install records the receipt checked below.
    const local = npmInstall(false)
    check(`a local npm install records no installation (foreground=${foreground})`,
      local.status === 0 && !existsSync(dir), `${local.status} ${local.stderr}`)
    const result = npmInstall(true)
    check(`real npm lifecycle succeeds (foreground=${foreground})`, result.status === 0, `${result.status} ${result.stderr}`)
    const prefs = join(dir, 'preferences.json')
    if (foreground) {
      check('foreground npm discloses before enabling statistics', existsSync(prefs) && (result.stdout + result.stderr).includes('AGIT_TELEMETRY_DISABLED'), result.stdout + result.stderr)
      check('foreground npm records a verified install without setup or registration', existsSync(prefs) && JSON.parse(readFileSync(prefs, 'utf8')).install_reported === true)
    } else {
      check('hidden npm output cannot enable statistics or allocate an installation ID', !existsSync(prefs) && !existsSync(join(dir, 'queue.json')) && !(result.stdout + result.stderr).includes('AGIT_TELEMETRY_DISABLED'))
      const pending = join(dir, 'pending-install.json')
      check('hidden npm retains the verified installation fact', existsSync(pending))
      if (existsSync(pending)) {
        const fact = JSON.parse(readFileSync(pending, 'utf8'))
        const consent = spawnSync(bin, ['setup', '--yes', '--skill', '--runtime', 'codex'], { encoding: 'utf8', env: lifecycleEnv, cwd: consumer })
        const queuePath = join(dir, 'queue.json')
        const receipt = existsSync(queuePath) && JSON.parse(readFileSync(queuePath, 'utf8')).entries.find(entry => entry.event.event === 'cli_install_succeeded')?.event
        check('visible consent records the original installation without registration', consent.status === 0 && consent.stderr.includes('AGIT_TELEMETRY_DISABLED') && receipt?.timestamp === fact.verified_at, consent.stderr)
      }
    }
  }
}

// 3. source checkout: postinstall must skip itself on this path
{
  const r = spawnSync('node', [join(root, 'npm', 'postinstall.js')], { encoding: 'utf8', env })
  check('postinstall in a source checkout stays silent', r.status === 0, `${r.status} ${r.stderr}`)
}

// Once agit has returned, nothing of it may hold the folder it worked in: a person deleting that
// folder would meet the same refusal. Record what is alive and fail at once, without retrying.
function processesThatMayHoldIt() {
  if (process.platform !== 'win32') return '(not listed on this platform)'
  const script = `$work = $env:AGIT_SMOKE_WORK
Get-CimInstance Win32_Process |
  Where-Object { $_.Name -in 'agit.exe', 'git.exe', 'node.exe' -or ($_.CommandLine -and $_.CommandLine.Contains($work)) -or ($_.ExecutablePath -and $_.ExecutablePath.Contains($work)) } |
  ForEach-Object { "pid $($_.ProcessId) parent $($_.ParentProcessId) $($_.Name): $($_.CommandLine)" }`
  const listed = spawnSync('powershell', ['-NoProfile', '-Command', script], { encoding: 'utf8', env: { ...process.env, AGIT_SMOKE_WORK: work } })
  return listed.status === 0 ? listed.stdout.trim() || '(none)' : `(could not list: ${listed.error ?? listed.stderr})`
}
try {
  rmSync(work, { recursive: true, force: true })
} catch (error) {
  check('the work folder can be removed once agit has returned', false, `${error.code} ${error.path}; processes that may hold it:\n${processesThatMayHoldIt()}`)
}
console.log(failed === 0 ? 'smoke ok' : `smoke FAILED (${failed})`)
process.exit(failed === 0 ? 0 : 1)
