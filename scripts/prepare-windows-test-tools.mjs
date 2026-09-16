import { createHash } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');

export async function prepareArchive(tool, root, fetchArchive = fetch) {
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
  const root = resolve(process.argv[2] || '.');
  const requested = process.argv[3];
  if (requested && !tools[requested]) throw new Error(`Unknown Windows tool: ${requested}`);
  const selected = requested ? [tools[requested]] : Object.values(tools);
  await Promise.all(selected.map(async (tool) => {
    console.log(`Verified ${await prepareArchive(tool, root)}`);
  }));
}
