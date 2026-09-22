#!/usr/bin/env node
/**
 * The `npx create-agit` half: a durable single-file install.
 *
 * The npx cache is wiped, and what the user gets has to be an `agit` that keeps working, so
 * this copies the platform binary out of the dependency tree (an optional dep of
 * @einsia/agent-git, already picked by npm for this os/cpu) to ~/.local/bin/agit, then runs
 * `agit setup` once to install skills / hooks / MCP. Zero network — the real download already
 * happened when npx fetched the package.
 *
 * An unsupported platform (no matching platform package) leaves nothing half-installed: print
 * the build-from-source instructions and exit.
 */

import { createRequire } from 'node:module'
import { randomUUID } from 'node:crypto'
import { performance } from 'node:perf_hooks'
import { chmodSync, copyFileSync, mkdirSync } from 'node:fs'
import { homedir, platform, arch } from 'node:os'
import { join, delimiter } from 'node:path'
import { spawnSync } from 'node:child_process'

const require = createRequire(import.meta.url)

function say(m) { console.log(m) }
function dim(m) { console.log(`\x1b[2m${m}\x1b[0m`) }
function ok(m) { console.log(`\x1b[32m✓\x1b[0m ${m}`) }
function fail(m) { console.error(`\x1b[31m✗ ${m}\x1b[0m`) }

const { packageKey, binaryName } = require('@einsia/agent-git/npm/lib/platform.js')

function main() {
  const args = process.argv.slice(2)
  const attributionIndex = args.indexOf('--acquisition-id')
  const acquisitionId = attributionIndex >= 0 ? args[attributionIndex + 1] : undefined
  if (attributionIndex >= 0 && !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(acquisitionId ?? '')) {
    fail('--acquisition-id requires a random UUID.')
    process.exit(1)
  }
  const campaignIndex = args.indexOf('--campaign-url')
  const campaignUrl = campaignIndex >= 0 ? args[campaignIndex + 1] : undefined
  if (campaignIndex >= 0) {
    try {
      if (!campaignUrl || campaignUrl.length > 8192 || !['http:', 'https:'].includes(new URL(campaignUrl).protocol)) throw new Error()
    } catch {
      fail('--campaign-url requires an HTTP or HTTPS URL of at most 8192 characters.')
      process.exit(1)
    }
  }
  const t = packageKey()
  if (!t) {
    fail(`no prebuilt binary for ${platform()}/${arch()}.`)
    say('Build from source instead:')
    say('  git clone https://github.com/Einsia/agent-git && cd agent-git && ./setup.sh')
    process.exit(1)
  }

  let bin
  try {
    bin = require.resolve(`@einsia/agent-git-${t}/bin/${binaryName()}`)
  } catch {
    fail(`platform package @einsia/agent-git-${t} is missing (npm skipped it as an optional dep?).`)
    say('If you are on an unsupported platform, build from source:')
    say('  git clone https://github.com/Einsia/agent-git && cd agent-git && ./setup.sh')
    process.exit(1)
  }

  say('agent-git installer')

  const yes = ['1', 'true'].includes(process.env.npm_config_yes?.toLowerCase()) || args.some(a => a === '--yes' || a === '-y')
  const installerEnv = { ...process.env, AGIT_INSTALL_CHANNEL: 'create_agit', AGIT_INSTALLER_YES: yes ? '1' : '0',
    ...(acquisitionId ? { AGIT_ACQUISITION_ID: acquisitionId } : {}),
    ...(campaignUrl ? { AGIT_CAMPAIGN_URL: campaignUrl } : {}) }
  const attemptId = randomUUID()
  const started = performance.now()
  const report = (stage, outcome, since = started, errorCategory = 'none') => {
    spawnSync(bin, ['--internal-install-stage', JSON.stringify({
      attempt_id: attemptId, stage, outcome, elapsed_ms: Math.round(performance.now() - since), error_category: errorCategory,
    })], { stdio: 'inherit', env: installerEnv, ...(stage === 'started' ? {} : { timeout: 2000 }) })
  }
  report('started', 'started')
  installerEnv.AGIT_INSTALLER_ONBOARDING_HANDLED = '1'
  const targetDir = join(homedir(), '.local', 'bin')
  const target = join(targetDir, binaryName())
  const copying = performance.now()
  try {
    mkdirSync(targetDir, { recursive: true })
    copyFileSync(bin, target)
    chmodSync(target, 0o755)
    report('binary_copy', 'ok', copying)
  } catch (error) {
    report('binary_copy', 'error', copying, 'filesystem')
    report('finished', 'error', started, 'filesystem')
    fail(`could not install the binary: ${error.message}`)
    process.exit(1)
  }

  const verifying = performance.now()
  const check = spawnSync(target, ['--version'], { encoding: 'utf8', env: { ...process.env, AGIT_TELEMETRY_DEFER: '1' } })
  if (check.error || check.status !== 0) {
    report('verification', 'error', verifying, 'binary')
    report('finished', 'error', started, 'binary')
    fail(`the binary did not run: ${(check.stderr || check.error?.message || `exit ${check.status}`).trim()}`)
    process.exit(1)
  }
  report('verification', 'ok', verifying)
  ok(`installed ${check.stdout.trim()} → ${target}`)

  if (!process.env.PATH?.split(delimiter).includes(targetDir)) {
    dim(`note: ${targetDir} is not on your PATH — add it to your ${platform() === 'win32' ? 'user environment variables' : 'shell profile'}`)
  }

  // "installed by default at download time": hooks + skill + MCP + AGENTS.md in one pass. A
  // failure does not block the install itself — the binary is already there, and setup can be
  // re-run by hand later.
  spawnSync(target, ['--internal-install-completed', '--defer-notice'], {
    stdio: ['ignore', 'inherit', 'inherit'], env: installerEnv,
  })
  const skipSetup = process.env.AGIT_SKIP_SETUP && !['0', 'false'].includes(process.env.AGIT_SKIP_SETUP.toLowerCase())
  if (skipSetup) {
    report('setup', 'skipped', performance.now())
    report('finished', 'ok')
    dim('Setup skipped by AGIT_SKIP_SETUP. Run `agit setup` when ready.')
    return
  }
  const integrating = performance.now()
  const setup = spawnSync(target, yes ? ['setup', '--yes'] : ['setup'], {
    stdio: 'inherit',
    env: installerEnv,
  })
  report('setup', setup.status === 0 ? 'ok' : 'error', integrating, setup.status === 0 ? 'none' : 'integration')
  report('finished', setup.status === 0 ? 'ok' : 'partial', started, setup.status === 0 ? 'none' : 'integration')
  if (setup.status === 0) {
    ok('integrations installed (skills · hooks · MCP · AGENTS.md)')
  } else {
    dim('`agit setup` did not fully succeed — re-run it later; the CLI itself is installed.')
  }

  say('')
  say('Next:')
  say('  agit login        # sign in to the hub')
  say('  then ask your agent to follow https://agent-git.com/docs/quickstart/')
  say('')
  dim('docs: https://agent-git.com/docs')
}

main()
