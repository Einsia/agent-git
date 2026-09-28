import { randomUUID } from 'node:crypto'
import { chmodSync, copyFileSync, existsSync, mkdirSync, renameSync, rmSync, constants } from 'node:fs'
import { dirname, join } from 'node:path'

export function installBinary(source, target, windows = process.platform === 'win32') {
  const directory = dirname(target)
  const extension = windows ? '.exe' : ''
  const staging = join(directory, `.agit-install-${randomUUID()}${extension}`)
  mkdirSync(directory, { recursive: true })
  try {
    copyFileSync(source, staging, constants.COPYFILE_EXCL)
    chmodSync(staging, 0o755)
    if (windows && existsSync(target)) {
      const previous = join(directory, `.agit-previous-${randomUUID()}.exe`)
      renameSync(target, previous)
      try {
        renameSync(staging, target)
      } catch (error) {
        renameSync(previous, target)
        throw error
      }
      try { rmSync(previous) } catch { /* A running Windows image can retain its old path. */ }
    } else {
      renameSync(staging, target)
    }
  } finally {
    try { rmSync(staging) } catch { /* The staged file may already have moved. */ }
  }
}
