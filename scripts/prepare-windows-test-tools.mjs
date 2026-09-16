import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');

async function fetchArchiveWithCurl(url, { signal }) {
  const { stdout } = await promisify(execFile)('curl', [
    '--fail', '--location', '--silent', '--show-error', '--max-time', '180', url,
  ], { signal, encoding: 'buffer', maxBuffer: 128 * 1024 * 1024 });
  return new Response(stdout);
}

export async function prepareArchive(tool, root, fetchArchive = fetchArchiveWithCurl) {
  const path = join(root, tool.cacheDirectory, `${tool.sha256}-${tool.archive}`);
  const cached = await readFile(path).catch((error) => {
    if (error.code !== 'ENOENT') throw error;
  });
  if (cached && digest(cached) === tool.sha256) return path;

  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      const response = await fetchArchive(tool.url, { signal: AbortSignal.timeout(180_000) });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const bytes = Buffer.from(await response.arrayBuffer());
      if (digest(bytes) !== tool.sha256) throw new Error('archive checksum mismatch');
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, bytes);
      return path;
    } catch (error) {
      console.error(`${tool.archive}, attempt ${attempt}: ${error.message}`);
      if (attempt === 3) throw error;
      await delay(2000);
    }
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const tools = JSON.parse(await readFile(new URL('./windows-test-tools.json', import.meta.url), 'utf8'));
  tools.python = JSON.parse(await readFile(new URL('./windows-build-python.json', import.meta.url), 'utf8'));
  const runtime = JSON.parse(await readFile(new URL('./git-runtime-lock.json', import.meta.url), 'utf8'));
  for (const [name, pin] of Object.entries({ git: runtime.windows, bundledLfs: runtime.lfs['windows-amd64'] })) {
    tools[name] = { ...pin, archive: pin.url.split('/').at(-1), cacheDirectory: '.cache/test-lfs' };
  }
  const root = resolve(process.argv[2] || '.');
  const requested = process.argv[3];
  if (requested && !tools[requested]) throw new Error(`Unknown Windows tool: ${requested}`);
  const selected = requested ? [tools[requested]] : [...new Map(Object.values(tools).map(tool => [tool.sha256, tool])).values()];
  const mirror = process.env.CI_JOB_TOKEN && process.env.CI_API_V4_URL && process.env.CI_PROJECT_ID
    ? `${process.env.CI_API_V4_URL}/projects/${process.env.CI_PROJECT_ID}/packages/generic/windows-ci-tools`
    : null;
  await Promise.all(selected.map(async (tool) => {
    const source = mirror ? { ...tool, url: `${mirror}/${tool.sha256}/${tool.archive}` } : tool;
    const download = mirror
      ? (url, options) => fetch(url, { ...options, headers: { 'JOB-TOKEN': process.env.CI_JOB_TOKEN } })
      : fetchArchiveWithCurl;
    console.log(`Verified ${await prepareArchive(source, root, download)}`);
  }));
}
