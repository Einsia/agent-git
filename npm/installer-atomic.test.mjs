import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { installBinary } from './create-agit/install-binary.mjs'

test('an installer failure preserves the prior executable and a retry publishes the replacement', () => {
  const directory = mkdtempSync(join(tmpdir(), 'agit-install-'))
  try {
    const target = join(directory, 'agit')
    const source = join(directory, 'new-agit')
    writeFileSync(target, 'old')
    assert.throws(() => installBinary(join(directory, 'missing'), target))
    assert.equal(readFileSync(target, 'utf8'), 'old')
    writeFileSync(source, 'new')
    installBinary(source, target)
    assert.equal(readFileSync(target, 'utf8'), 'new')
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})
