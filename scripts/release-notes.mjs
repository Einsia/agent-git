#!/usr/bin/env node
/**
 * Print one version's section of CHANGELOG.md: the body of that version's GitHub Release.
 *
 *   node scripts/release-notes.mjs 0.1.1
 *
 * The section starts at the `## [<version>]` heading and ends before the next `## ` heading.
 * The heading itself is not printed, since the Release carries the tag as its title. A missing
 * or empty section is an error: a release without notes must not go out, and
 * `scripts/check-version.js` and `scripts/bump-version.mjs` run this same check so the gap is
 * caught before the tag exists.
 *
 * A Release page resolves a relative link against `/releases/tag/`, where no file lives, so
 * every repository-relative link in the section is rewritten to the file as it is at the
 * release tag. The changelog itself keeps relative links, which work where the file is read.
 */
import { readFileSync, realpathSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * The section for `version`, with repository-relative links pointed at `repo` at `tag`.
 * Throws when the section is missing or empty.
 */
export function releaseNotes(changelog, version, { repo, tag }) {
  const lines = changelog.split(/\r?\n/)
  const start = lines.findIndex((l) => l.startsWith(`## [${version}]`))
  if (start < 0) {
    throw new Error(
      `CHANGELOG.md has no "## [${version}]" section; write one before releasing ${version}.`,
    )
  }
  let end = lines.findIndex((l, i) => i > start && l.startsWith('## '))
  if (end < 0) end = lines.length
  // Reference-style link definitions (`[0.1.1]: https://…`) belong to the file, not to a section.
  const body = lines
    .slice(start + 1, end)
    .filter((l) => !/^\[[^\]]+\]:\s+\S/.test(l))
    .join('\n')
    .trim()
  if (!body) throw new Error(`the "## [${version}]" section of CHANGELOG.md is empty.`)
  return pinLinks(body, repo, tag) + '\n'
}

// A link is left alone when it has a scheme (`https:`, `mailto:`) or points inside the page
// (`#anchor`); anything else names a file in the repository, with or without a leading slash.
function pinLinks(text, repo, tag) {
  return text.replace(/\]\(([^)\s]+)\)/g, (whole, target) => {
    if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('#')) return whole
    return `](${repo}/blob/${tag}/${target.replace(/^\/+/, '')})`
  })
}

/** `git+https://github.com/o/r.git` → `https://github.com/o/r`, the form a blob URL builds on. */
export function repoWebUrl(repositoryUrl) {
  return repositoryUrl.replace(/^git\+/, '').replace(/\.git$/, '').replace(/\/+$/, '')
}

const invokedDirectly =
  process.argv[1] && realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)
if (invokedDirectly) {
  const version = process.argv[2]
  if (!version) {
    console.error('usage: node scripts/release-notes.mjs <version>')
    process.exit(2)
  }
  const root = join(dirname(fileURLToPath(import.meta.url)), '..')
  const repo = repoWebUrl(JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).repository.url)
  try {
    process.stdout.write(
      releaseNotes(readFileSync(join(root, 'CHANGELOG.md'), 'utf8'), version, {
        repo,
        tag: `agit-v${version}`,
      }),
    )
  } catch (e) {
    console.error(`[release-notes] ${e.message}`)
    process.exit(1)
  }
}
