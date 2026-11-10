import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, utimesSync, rmSync } from 'node:fs'
import * as fs from 'node:fs'
import { runInNewContext } from 'node:vm'
import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import path from 'node:path'
import os from 'node:os'

const runScript = readFileSync(new URL('./lib/run.js', import.meta.url), 'utf8')
const workerScript = readFileSync(new URL('./lib/reconcile-worker.js', import.meta.url), 'utf8')
const scheduleScript = readFileSync(new URL('./lib/reconcile.js', import.meta.url), 'utf8')

test('a skipped install script retries after the daemon changes without delaying the CLI', () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'agit-npm-reconcile-'))
  try {
    const dir = path.join(home, 'desktop-rc')
    mkdirSync(dir)
    const pid = path.join(dir, 'agitd.pid')
    const bin = path.join(home, 'agit')
    writeFileSync(bin, 'binary')
    const calls = []
    const context = {
      __dirname: '/global/node_modules/@einsia/agent-git/npm/lib',
      module: { exports: {} },
      process: { argv: ['node', '/global/bin/agit'], env: { AGIT_HOME: home }, execPath: '/node', pid: 12 },
      require: (name) => ({
        child_process: { spawn: (_node, args, options) => {
          calls.push({ args: [...args], detached: options.detached, stdio: options.stdio })
          return { on: () => {}, unref: () => {} }
        } },
        fs,
        os,
        path,
      })[name],
    }
    runInNewContext(scheduleScript, context)
    const schedule = context.module.exports.scheduleReconciliation
    schedule(bin, '/global/node_modules/@einsia/agent-git/npm')
    assert.equal(calls.length, 0)
    writeFileSync(pid, '123')
    schedule(bin, '/global/node_modules/@einsia/agent-git/npm')
    schedule(bin, '/global/node_modules/@einsia/agent-git/npm')
    assert.equal(calls.length, 1)
    assert.equal(calls[0].detached, true)
    assert.equal(calls[0].stdio, 'ignore')
    const future = new Date(Date.now() + 10_000)
    utimesSync(pid, future, future)
    schedule(bin, '/global/node_modules/@einsia/agent-git/npm')
    assert.equal(calls.length, 2)
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})

test('the npm shim preserves command semantics while scheduling a daemon check', () => {
  const calls = []
  const context = {
    __dirname: '/global/node_modules/@einsia/agent-git/npm/lib',
    module: { exports: {} },
    process: {
      argv: ['node', 'agit', 'status'], env: {}, platform: 'linux', arch: 'x64',
      exit: (code) => { throw { exitCode: code } },
    },
    require: (name) => ({
      child_process: { spawnSync: (bin, args, options) => {
        calls.push({ bin, args: [...args], stdio: options.stdio })
        return { status: 7 }
      } },
      path,
      './platform': {},
      './resolve': { resolveBinary: () => '/global/bin/agit' },
      './log': { error: () => {} },
      './reconcile': { scheduleReconciliation: (bin, root) => calls.push({ bin, root }) },
    })[name],
  }
  assert.throws(() => runInNewContext(`${runScript}\nrun()`, context), error => error.exitCode === 7)
  assert.deepEqual(calls[0], { bin: '/global/bin/agit', args: ['status'], stdio: 'inherit' })
  assert.deepEqual(calls[1], {
    bin: '/global/bin/agit', root: '/global/node_modules/@einsia/agent-git/npm',
  })
})

// A run the system killed (out of memory, most often) says so on stderr and exits as a shell
// would, 128 plus the signal's number; a person's Ctrl-C exits the same way without a line. A
// shim that turned every signal into a bare exit 1 left a killed save looking like an ordinary
// failure with nothing said.
test('a run the system killed says so and exits as a shell would', () => {
  const said = []
  const outcome = (signal) => {
    const context = {
      __dirname: '/global/node_modules/@einsia/agent-git/npm/lib',
      module: { exports: {} },
      process: {
        argv: ['node', 'agit', 'commit'], env: {}, platform: 'linux', arch: 'x64',
        exit: (code) => { throw { exitCode: code } },
      },
      require: (name) => ({
        child_process: { spawnSync: () => ({ status: null, signal }) },
        os,
        path,
        './platform': {},
        './resolve': { resolveBinary: () => '/global/bin/agit' },
        './log': { error: (message) => said.push(message) },
        './reconcile': { scheduleReconciliation: () => {} },
      })[name],
    }
    let code
    assert.throws(() => runInNewContext(`${runScript}\nrun()`, context), error => {
      code = error.exitCode
      return true
    })
    return code
  }
  assert.equal(outcome('SIGKILL'), 137)
  assert.equal(said.length, 1)
  assert.match(said[0], /stopped by the system.*ran out of memory/)
  assert.equal(outcome('SIGINT'), 130)
  assert.equal(said.length, 1)
})

test('a transient package cannot replace the persistent daemon', () => {
  const calls = []
  const context = {
    process: {
      argv: ['node', 'worker', '/binary/agit', '/project/node_modules/@einsia/agent-git'],
      env: {}, platform: 'linux', exit: (code) => { throw { exitCode: code } },
    },
    require: (name) => ({
      child_process: { spawnSync: (bin, args) => {
        calls.push({ bin, args: [...args] })
        return bin === 'npm' ? { status: 0, stdout: '/global/node_modules\n' } : { status: 0 }
      } },
      fs: { realpathSync: value => value },
      path,
    })[name],
  }
  assert.throws(() => runInNewContext(workerScript, context), error => error.exitCode === 0)
  assert.deepEqual(calls, [{ bin: 'npm', args: ['root', '-g'] }])
  context.process.argv[3] = '/global/node_modules/@einsia/agent-git/npm'
  runInNewContext(workerScript, { ...context })
  assert.deepEqual(calls[2], {
    bin: '/binary/agit', args: ['rc', 'local', 'reconcile-installed', '--allow-other-installation'],
  })
})

test('a verified global bin symlink survives a Node manager switch', () => {
  const calls = []
  runInNewContext(workerScript, {
    process: {
      argv: ['node', 'worker', '/binary/agit', '/prefix/lib/node_modules/@einsia/agent-git/npm', '/prefix/bin/agit'],
      env: {}, platform: 'linux', exit: () => { throw new Error('unexpected exit') },
    },
    require: (name) => ({
      child_process: { spawnSync: (bin, args) => {
        calls.push({ bin, args: [...args] })
        return bin === 'npm' ? { status: 1 } : { status: 0 }
      } },
      fs: {
        realpathSync: value => value === '/prefix/bin/agit'
          ? '/prefix/lib/node_modules/@einsia/agent-git/npm/shim.js' : value,
        lstatSync: () => ({ isSymbolicLink: () => true }),
      },
      path,
    })[name],
  })
  assert.deepEqual(calls[1].args, ['rc', 'local', 'reconcile-installed', '--allow-other-installation'])
})
