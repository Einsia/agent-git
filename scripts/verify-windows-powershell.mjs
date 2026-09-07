// The native retry host must execute its script before a CLI integration result is meaningful.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

if (process.platform !== 'win32') {
  console.error('PowerShell host verification requires native Windows.');
  process.exit(2);
}
const root = mkdtempSync(join(tmpdir(), 'agit-powershell-host-'));
const home = join(root, 'home');
const work = join(root, 'work');
mkdirSync(home);
mkdirSync(work);
const inherited = { ...process.env };
const lookup = (name) => Object.entries(process.env).find(([key]) => key.toUpperCase() === name.toUpperCase())?.[1];
const fixture = {
  PATH: lookup('PATH') || '', HOME: home, USERPROFILE: home,
  AGIT_HOME: join(root, 'agit'), AGIT_HUB_URL: 'http://127.0.0.1:1',
  AGIT_SECRETS_KEYSTORE: 'os', GIT_CONFIG_GLOBAL: join(home, 'empty-gitconfig'),
  GIT_CONFIG_NOSYSTEM: '1', GIT_TERMINAL_PROMPT: '0', CI: '1', NO_COLOR: '1',
};
for (const key of ['SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'ComSpec']) {
  const value = lookup(key);
  if (value !== undefined) fixture[key] = value;
}
const script = "$ErrorActionPreference = 'Stop'; [Console]::Out.WriteLine('SYNTHETIC-POWERSHELL-OUT'); [Console]::Error.WriteLine('SYNTHETIC-POWERSHELL-ERR'); exit 31";
const rows = [];
try {
  for (const [environment, env] of [['fixture', fixture], ['inherited', inherited]]) {
    for (const encoding of ['command', 'encoded']) {
      for (const stdin of ['ignore', 'pipe']) {
        const args = ['-NoProfile', '-NonInteractive', encoding === 'command' ? '-Command' : '-EncodedCommand',
          encoding === 'command' ? script : Buffer.from(script, 'utf16le').toString('base64')];
        const result = spawnSync('powershell', args, {
          cwd: work, env, windowsHide: true, stdio: [stdin, 'pipe', 'pipe'], timeout: 30000,
        });
        rows.push({ environment, encoding, stdin, status: result.status,
          failedToStart: Boolean(result.error),
          stdoutMarker: result.stdout?.toString('utf8').includes('SYNTHETIC-POWERSHELL-OUT') ?? false,
          stderrMarker: result.stderr?.toString('utf8').includes('SYNTHETIC-POWERSHELL-ERR') ?? false,
        });
      }
    }
  }
  console.log(JSON.stringify({ program: 'powershell', rows }, null, 2));
  const nativeHost = rows.find((row) => row.environment === 'inherited' && row.encoding === 'command' && row.stdin === 'pipe');
  if (nativeHost?.status !== 31 || !nativeHost.stdoutMarker || !nativeHost.stderrMarker || nativeHost.failedToStart) {
    throw new Error('The PowerShell host did not execute the native retry launch protocol');
  }
} finally {
  rmSync(root, { recursive: true, force: true });
}
