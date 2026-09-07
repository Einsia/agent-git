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

  const targetDir = join(homedir(), '.local', 'bin')
  mkdirSync(targetDir, { recursive: true })
  const target = join(targetDir, binaryName())
  copyFileSync(bin, target)
  chmodSync(target, 0o755)

  const check = spawnSync(target, ['--version'], { encoding: 'utf8' })
  if (check.error || check.status !== 0) {
    fail(`the binary did not run: ${(check.stderr || check.error?.message || `exit ${check.status}`).trim()}`)
    process.exit(1)
  }
  ok(`installed ${check.stdout.trim()} → ${target}`)

  if (!process.env.PATH?.split(delimiter).includes(targetDir)) {
    dim(`note: ${targetDir} is not on your PATH — add it to your ${platform() === 'win32' ? 'user environment variables' : 'shell profile'}`)
  }

  // "installed by default at download time": hooks + skill + MCP + AGENTS.md in one pass. A
  // failure does not block the install itself — the binary is already there, and setup can be
  // re-run by hand later.
  const setup = spawnSync(target, ['setup'], { stdio: 'inherit' })
  if (setup.status === 0) {
    ok('integrations installed (skills · hooks · MCP · AGENTS.md)')
  } else {
    dim('`agit setup` did not fully succeed — re-run it later; the CLI itself is installed.')
  }

  say('')
  say('Next:')
  say('  agit login        # sign in to the hub')
  say('  agit import <session-id> -n <name> -b <branch>   # sessions live on their own line')
  say('  agit push         # publish')
  say('')
  dim('docs: https://agent-git.com/docs')
}

main()
