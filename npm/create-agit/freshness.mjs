/**
 * Whether this create-agit is the newest one its registry publishes, and handing over to the
 * newest when it is not.
 *
 * npx resolves a name-only `create-agit` from its own cache, and uses a copy installed globally
 * or in the current project as it is, without asking the registry. Each create-agit installs the
 * agit of its own version, so an old installer keeps installing an old agit. A stale installer
 * therefore runs the newest one, once, through the npm that launched it, and an installer that
 * cannot ask the registry within the bound, or was not launched by npm, installs its own
 * version.
 */

/** Set for the handed-over installer, so it never hands over again. */
export const REEXEC = 'AGIT_CREATE_AGIT_REEXEC'

const RELEASE = /^\d+\.\d+\.\d+$/

/** Whether release `candidate` is newer than release `current`. */
export function newer(candidate, current) {
  const [a, b] = [candidate, current].map(version => version.split('.').map(Number))
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i]
  }
  return false
}

/**
 * The newest create-agit release the registry npm is configured with publishes, or null when it
 * cannot say within `timeoutMs`.
 */
export async function latestVersion(env, fetchImpl = fetch, timeoutMs = 3000) {
  const registry = (env.npm_config_registry || 'https://registry.npmjs.org/').replace(/\/?$/, '/')
  try {
    const response = await fetchImpl(`${registry}create-agit/latest`, {
      headers: { accept: 'application/json' },
      signal: AbortSignal.timeout(timeoutMs),
    })
    if (!response.ok) return null
    const { version } = await response.json()
    return typeof version === 'string' && RELEASE.test(version) ? version : null
  } catch {
    return null
  }
}

/**
 * The program and arguments that run create-agit@`version` with `args` through the npm CLI that
 * launched this installer, or null when npm did not launch it. Arguments are passed as they are,
 * never through a shell.
 */
export function handOverCommand(env, execPath, version, args) {
  const cli = env.npm_execpath
  if (!cli || !/(^|\/)npm-cli\.js$/.test(cli.replaceAll('\\', '/'))) return null
  return [execPath, [cli, 'exec', '--yes', `--package=create-agit@${version}`, '--', 'create-agit', ...args]]
}

/**
 * How fresh this installer is: `current`, `stale` (a newer one exists and this one installs
 * anyway), `unknown` (the registry did not answer), or `reexec` (this is the newer one a stale
 * installer handed over to). When a handover runs and succeeds, answers `{ exit: 0 }` instead,
 * and the stale installer must stop.
 */
export async function freshness({ env, own, args, execPath, fetchImpl, run, note }) {
  if (env[REEXEC] === '1') return 'reexec'
  if (!RELEASE.test(own)) return 'unknown'
  const latest = await latestVersion(env, fetchImpl)
  if (!latest) return 'unknown'
  if (!newer(latest, own)) return 'current'
  const command = handOverCommand(env, execPath, latest, args)
  if (!command) return 'stale'
  note(`create-agit ${own} is not the newest; running create-agit@${latest} instead`)
  const status = run(command[0], command[1], { ...env, [REEXEC]: '1' })
  if (status === 0) return { exit: 0 }
  note(`create-agit@${latest} did not finish${status == null ? '' : ` (exit ${status})`}; installing ${own}`)
  return 'stale'
}
