#!/usr/bin/env node
/**
 * agit's npm publisher, publishing the whole family:
 *
 *   @einsia/agent-git-{linux-x64,linux-arm64,darwin-x64,darwin-arm64}
 *   @einsia/agent-git          (optionalDependencies points at the four above)
 *   create-agit                (the one-shot `npx create-agit` install wrapper, depends on the main package)
 *
 * Usage:
 *
 *   gh release download agit-v<version> -p 'agit-*.tar.gz' -D <dir>
 *   node npm/publish.mjs <artifacts-dir> [--dry-run] [--dist-tag <tag>] [--only platforms|entry]
 *
 * What it does (the order is deliberate — the platform versions the main package's
 * optionalDependencies point at must exist on the registry first, or whoever installs gets a
 * 404; the install wrapper must likewise come after the main package):
 *
 *   1. Unpack the four platform binaries into platforms/<key>/bin/agit
 *   2. Sync version and dependency pins across every package.json (checked against Cargo.toml)
 *   3. npm publish: platform packages → main package → create-agit
 *
 * `--only` is for the npm-publish workflow: between the platform packages and the rest it
 * inserts a "really install the main package tarball" preflight, and that preflight requires
 * the platform packages to already be on the registry, so the publish plan is cut down the
 * middle into platforms / entry. Publishing the whole family by hand locally leaves this flag
 * off.
 */

import { execFileSync, spawnSync } from 'node:child_process'
import { existsSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const [artifacts, ...flags] = process.argv.slice(2)
const dry = flags.includes('--dry-run')
// For a rehearsal, or for publishing one platform first to verify the pipeline. A real release
// must carry all four — a missing platform means its users install and land on the "no prebuilt
// binary" error page.
const partial = flags.includes('--allow-partial')

function flagValue(name) {
  const i = flags.indexOf(name)
  if (i === -1) return null
  const v = flags[i + 1]
  if (!v || v.startsWith('--')) {
    console.error(`${name} needs a value`)
    process.exit(1)
  }
  return v
}
// The dist-tag decides who an `npm install` with no version number gets. For a limited trial
// first, publish to `next`, then move `latest` with `npm dist-tag add` once it checks out.
const distTag = flagValue('--dist-tag') || 'latest'
const only = flagValue('--only')
if (only && only !== 'platforms' && only !== 'entry') {
  console.error(`--only takes "platforms" or "entry", not "${only}"`)
  process.exit(1)
}

function run(cmd, args, opts = {}) {
  if (dry && args.includes('publish')) {
    console.log(`  [dry] ${cmd} ${args.join(' ')}  (${opts.cwd || root})`)
    return ''
  }
  return execFileSync(cmd, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], ...opts })
}

if (!artifacts || !existsSync(artifacts)) {
  console.error('usage: node npm/publish.mjs <artifacts-dir-with-agit-tarballs> [--dry-run] [--dist-tag <tag>] [--only platforms|entry]')
  process.exit(1)
}

// Cargo.toml decides the version (the single source of truth for the release flow); the
// artifact filenames must agree.
const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  .match(/\[package\][\s\S]*?version\s*=\s*"([^"]+)"/)?.[1]
if (!cargo) {
  console.error('Cargo.toml has no [package] version')
  process.exit(1)
}
const version = cargo
console.log(`version ${version}`)

const TRIPLES = {
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
}

// "has this been published" deliberately does not pre-query the registry: the packument
// replicates with a delay (`npm view` returns 404 for a while on a just-published package), so
// a pre-query is unreliable. Publish directly and read the "cannot overwrite a published
// version" E403 as idempotent success.
function tryPublish(dir) {
  const p = spawnSync3('npm', ['publish', '--access', 'public', '--tag', distTag], { cwd: dir })
  if (p.ok) return 'published'
  if (/previously published/.test(p.stderr)) return 'already-there'
  console.error(p.stderr || `npm publish exited ${p.status}`)
  process.exit(1)
}
function spawnSync3(cmd, args, opts) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', cwd: opts.cwd })
  return { ok: r.status === 0, status: r.status, stderr: r.stderr || String(r.error || '') }
}

for (const [key, triple] of Object.entries(TRIPLES)) {
  const dir = join(root, 'npm', 'platforms', key)
  const stagedBin = join(dir, 'bin', 'agit')
  const tarball = join(artifacts, `agit-${version}-${triple}.tar.gz`)
  if (!existsSync(tarball)) {
    // This artifacts directory carries no tar for this platform: any stale binary sitting in
    // the staged package directory must be cleared before deciding whether to publish —
    // otherwise the 0.8.1 bin goes out under the 0.8.2 package name.
    if (existsSync(stagedBin)) rmSync(stagedBin)
    if (partial) {
      console.log(`  [partial] skipping ${key} (no artifact)`)
      continue
    }
    console.error(`missing artifact: ${tarball}  (--allow-partial to publish a subset)`)
    process.exit(1)
  }
  run('tar', ['-xzf', tarball, '-C', join(dir, 'bin')], {})
  const bin = join(dir, 'bin', 'agit')
  // If it runs (native architecture), check the version; a cross-architecture artifact falls
  // back to the ELF magic check — release.yml's QEMU smoke test is the backstop for
  // cross-compilation correctness, so this does not pretend it can exec.
  let out
  try {
    out = run(bin, ['--version'], {}).trim()
  } catch {
    // ELF is linux; Mach-O (darwin) is one of a handful of magics — cf fa ed fe / ca fe ba be
    // (fat). The platform key and the magic are checked against each other, so a cross artifact
    // staged into the wrong slot (a musl binary inside a darwin package) shows up here.
    const head = readFileSync(bin).subarray(0, 4)
    const hex = head.toString('hex')
    const isELF = hex === '7f454c46'
    const isMachO = ['cffaedfe', 'cefaedfe', 'feedfacf', 'feedface', 'cafebabe'].includes(hex)
    const wantMachO = key.startsWith('darwin')
    if ((wantMachO && !isMachO) || (!wantMachO && !isELF)) {
      console.error(`${key} artifact neither runs nor has the right magic (got ${hex}) — corrupt staging`)
      process.exit(1)
    }
    out = `(cross, ${wantMachO ? 'Mach-O' : 'ELF'} ok)`
  }
  if (!out.endsWith(` ${version}`) && !out.startsWith('(cross,')) {
    console.error(`${key} artifact reports "${out}", expected version ${version} — staging the wrong binary?`)
    process.exit(1)
  }
  const pkgPath = join(dir, 'package.json')
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'))
  pkg.version = version
  writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n')
  console.log(`staged ${key}: ${out}`)
}

// Version and dependency pins for the main package and the install wrapper
for (const rel of ['package.json', 'npm/create-agit/package.json']) {
  const p = join(root, rel)
  const pkg = JSON.parse(readFileSync(p, 'utf8'))
  pkg.version = version
  for (const dep of Object.keys(pkg.optionalDependencies || {})) {
    pkg.optionalDependencies[dep] = version
  }
  for (const dep of Object.keys(pkg.dependencies || {})) {
    if (dep === '@einsia/agent-git') pkg.dependencies[dep] = version
  }
  writeFileSync(p, JSON.stringify(pkg, null, 2) + '\n')
}

const staged = Object.keys(TRIPLES).filter((k) =>
  existsSync(join(root, 'npm', 'platforms', k, 'bin', 'agit')),
)
// optionalDependencies pointing at a platform version that is not published yet is safe: npm
// skips an optional dep that returns 404, and users on that machine only get the "no prebuilt
// binary" hint.
const platformPlan = staged.map((k) => [`@einsia/agent-git-${k}`, join(root, 'npm', 'platforms', k)])
const entryPlan = [
  ['@einsia/agent-git', root],
  ['create-agit', join(root, 'npm', 'create-agit')],
]
const plan =
  only === 'platforms' ? platformPlan : only === 'entry' ? entryPlan : [...platformPlan, ...entryPlan]

for (const [name, dir] of plan) {
  console.log(`publish ${name}@${version}`)
  if (dry) { console.log(`  [dry] npm publish  (${dir})`); continue }
  const outcome = tryPublish(dir)
  console.log(`  ${outcome === 'published' ? 'published' : 'already on the registry — skipped'}`)
}

console.log(dry ? 'dry run complete.' : 'done. https://www.npmjs.com/package/@einsia/agent-git')
