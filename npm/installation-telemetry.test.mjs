import { readFileSync } from 'node:fs'
import { runInNewContext } from 'node:vm'
import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import path from 'node:path'

const script = readFileSync(new URL('./postinstall.js', import.meta.url), 'utf8')
function install(env, versionStatus = 0) {
  const calls = []
  runInNewContext(script, {
    __dirname: '/fixture/node_modules/@einsia/agent-git',
    process: { env },
    require: (name) => ({
      child_process: { spawnSync: (_bin, args, options) => {
        calls.push({ args: [...args], env: { ...options.env } })
        return { status: args[0] === '--version' ? versionStatus : 0 }
      } },
      path,
      fs: { existsSync: () => false },
      './lib/resolve': { resolveBinary: () => '/fixture/agit' },
      './lib/log': { warn: () => {} },
    })[name],
  })
  return calls
}

const GLOBAL = { npm_config_global: 'true' }
const RECONCILE = ['rc', 'local', 'reconcile-installed', '--allow-other-installation']

test('hidden npm retains a verified install even when integration setup is skipped', () => {
  const calls = install({ ...GLOBAL, AGIT_SKIP_SETUP: '1', DO_NOT_TRACK: '1' })
  assert.deepEqual(calls.map(c => c.args), [['--version'], ['--internal-install-completed', '--defer-notice'], RECONCILE])
  assert.equal(calls[0].env.AGIT_TELEMETRY_DEFER, '1')
  assert.equal(calls[1].env.DO_NOT_TRACK, '1')
  assert.equal(calls[1].env.AGIT_INSTALL_CHANNEL, 'npm_global')
})

test('a failed binary self-check never reports installation success', () => {
  assert.deepEqual(install(GLOBAL, 1).map(c => c.args), [['--version']])
})

test('npm exec delegates the durable-install receipt and setup to create-agit', () => {
  assert.deepEqual(install({ ...GLOBAL, npm_command: 'exec' }).map(c => c.args), [['--version']])
})

// A local install (a project dependency, or `--prefix` into a scratch folder) only checks its
// binary. One that recorded a receipt or ran `agit setup` would configure the person's machine
// and point every runtime's hooks at a binary that goes away with that folder. Both ways npm
// marks a global install still set the machine up.
test('a local install records nothing and configures nothing', () => {
  assert.deepEqual(install({}).map(c => c.args), [['--version']])
  for (const env of [GLOBAL, { npm_config_location: 'global' }]) {
    assert.deepEqual(install(env).map(c => c.args).at(-1), ['setup'])
  }
})

test('foreground npm allows the verified binary to show its notice', () => {
  const calls = install({ ...GLOBAL, npm_config_foreground_scripts: 'true', AGIT_SKIP_SETUP: '1' })
  assert.deepEqual(calls[1].args, ['--internal-install-completed'])
})
