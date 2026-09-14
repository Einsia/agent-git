import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { prepareArchive } from './prepare-windows-test-tools.mjs';

const bytes = Buffer.from('synthetic verified tool archive');
const tool = {
  archive: 'tool.zip',
  sha256: createHash('sha256').update(bytes).digest('hex'),
  cacheDirectory: 'cache',
  url: 'https://example.invalid/tool.zip',
};

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'agit-test-tools-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, tool.cacheDirectory, `${tool.sha256}-${tool.archive}`);
  await mkdir(dirname(path), { recursive: true });
  return { root, path };
}

test('a verified cache requires no network request', async (t) => {
  const { root, path } = await fixture(t);
  await writeFile(path, bytes);
  assert.equal(await prepareArchive(tool, root, () => assert.fail('unexpected network request')), path);
});

test('corrupt cached bytes are replaced only by a verified response', async (t) => {
  const { root, path } = await fixture(t);
  await writeFile(path, 'corrupt archive');
  await prepareArchive(tool, root, async () => new Response(bytes));
  assert.deepEqual(await readFile(path), bytes);
});

test('a truncated response is retried without appending its bytes', async (t) => {
  const { root, path } = await fixture(t);
  let attempts = 0;
  await prepareArchive(tool, root, async () => new Response(++attempts === 1 ? bytes.subarray(0, 4) : bytes));
  assert.equal(attempts, 2);
  assert.deepEqual(await readFile(path), bytes);
});

test('exhausted download failures do not publish an archive', async (t) => {
  const { root, path } = await fixture(t);
  let attempts = 0;
  await assert.rejects(prepareArchive(tool, root, async () => {
    attempts++;
    return new Response('unavailable', { status: 503 });
  }), /HTTP 503/);
  assert.equal(attempts, 3);
  await assert.rejects(readFile(path), { code: 'ENOENT' });
});
