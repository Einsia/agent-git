import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import { REEXEC, freshness } from './freshness.mjs'

const NPM = '/usr/lib/node_modules/npm/bin/npm-cli.js'
const registry = (version) => async (url) => {
  assert.equal(url, 'https://mirror.example/create-agit/latest')
  return { ok: true, json: async () => ({ version }) }
}

function decide(overrides) {
  const runs = []
  const notes = []
  const result = freshness({
    env: { npm_config_registry: 'https://mirror.example', npm_execpath: NPM },
    own: '0.2.9',
    args: ['--acquisition-id', 'x', '--campaign-url', 'https://a.example/?q=1&r=2'],
    execPath: '/usr/bin/node',
    fetchImpl: registry('0.2.10'),
    run: (program, args, env) => { runs.push({ program, args, env }); return 0 },
    note: (line) => notes.push(line),
    ...overrides,
  })
  return { result, runs, notes }
}

// Pins that a stale installer hands over to the newest release exactly once: through the npm CLI
// that launched it (no shell, so arguments such as a campaign URL reach it unchanged), from the
// registry npm is configured with, marking the handover so the newer installer never hands over
// again, and stopping when it succeeds. Comparing versions as text (0.2.10 < 0.2.9), asking the
// public registry instead of the configured mirror, or handing over without the marker fails here.
test('a stale installer hands the install to the newest release once', async () => {
  const { result, runs } = decide()
  assert.deepEqual(await result, { exit: 0 })
  assert.equal(runs.length, 1)
  assert.equal(runs[0].program, '/usr/bin/node')
  assert.deepEqual(runs[0].args, [NPM, 'exec', '--yes', '--package=create-agit@0.2.10', '--', 'create-agit',
    '--acquisition-id', 'x', '--campaign-url', 'https://a.example/?q=1&r=2'])
  assert.equal(runs[0].env[REEXEC], '1')

  const handedOver = decide({ env: { [REEXEC]: '1', npm_execpath: NPM }, fetchImpl: () => assert.fail('asked the registry') })
  assert.equal(await handedOver.result, 'reexec')
  assert.equal(handedOver.runs.length, 0)
})

// Pins that the freshness check never stands between a person and an install: a registry that
// does not answer, a version it cannot compare, a launcher that is not npm, or a handover that
// fails all leave this installer installing its own version, and only a handover that fails runs
// anything. An installer that stopped, or retried the handover, in any of these would fail here.
test('an installer that cannot hand over installs its own version', async () => {
  for (const [overrides, expected] of [
    [{ fetchImpl: async () => { throw new Error('offline') } }, 'unknown'],
    [{ fetchImpl: async () => ({ ok: false }) }, 'unknown'],
    [{ own: '0.2.9-dev' }, 'unknown'],
    [{ fetchImpl: registry('0.2.9') }, 'current'],
    [{ env: { npm_config_registry: 'https://mirror.example', npm_execpath: '/opt/pnpm/bin/pnpm.cjs' } }, 'stale'],
  ]) {
    const { result, runs } = decide(overrides)
    assert.equal(await result, expected)
    assert.equal(runs.length, 0)
  }
  const failed = decide({ run: () => 1 })
  assert.equal(await failed.result, 'stale')
  assert.match(failed.notes.at(-1), /installing 0\.2\.9/)
})
