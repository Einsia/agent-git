#!/usr/bin/env node
// Regression for `release-notes.mjs`: a Release body is exactly one version's section, and every
// link in it survives the move from the changelog file to the Release page.
//
//   node --test scripts/release-notes.test.mjs
import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import { releaseNotes, repoWebUrl } from './release-notes.mjs'

const repo = 'https://github.com/Einsia/agent-git'
const tag = 'agit-v0.1.1'
const changelog = `# Changelog

Intro paragraph with a [link](docs/00_usage.md) that is not part of any section.

## [0.1.1] - 2026-09-04

### Added

- See [docs/07_tui.md](docs/07_tui.md) and [the boundary](docs/05_global_secret_filter.md#2-non-goals).
- Root-relative [LICENSE](/LICENSE), external [site](https://agent-git.com), [mail](mailto:x@y.z),
  and an in-page [anchor](#fixed).

## [0.1.0] - 2026-09-01

First public release.

## [0.0.9] - 2026-08-31

[0.1.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.1
[0.1.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.0
`

test('one section, heading dropped, link definitions left out', () => {
  const notes = releaseNotes(changelog, '0.1.0', { repo, tag: 'agit-v0.1.0' })
  assert.equal(notes, 'First public release.\n')
  const newer = releaseNotes(changelog, '0.1.1', { repo, tag })
  assert.ok(newer.startsWith('### Added'))
  assert.ok(!newer.includes('0.1.0'), 'the next section stays out')
  assert.ok(!newer.includes('releases/tag/agit-v0.1.1\n'), 'no link definition inside')
})

test('repository-relative links are pinned to the release tag; others are untouched', () => {
  const notes = releaseNotes(changelog, '0.1.1', { repo, tag })
  assert.ok(notes.includes(`](${repo}/blob/${tag}/docs/07_tui.md)`))
  assert.ok(notes.includes(`](${repo}/blob/${tag}/docs/05_global_secret_filter.md#2-non-goals)`))
  assert.ok(notes.includes(`](${repo}/blob/${tag}/LICENSE)`))
  assert.ok(notes.includes('](https://agent-git.com)'))
  assert.ok(notes.includes('](mailto:x@y.z)'))
  assert.ok(notes.includes('](#fixed)'))
  assert.ok(!notes.includes('](docs/'), 'no relative link survives')
})

test('a missing or empty section is an error, never an empty body', () => {
  assert.throws(() => releaseNotes(changelog, '0.2.0', { repo, tag }), /no "## \[0\.2\.0\]" section/)
  assert.throws(() => releaseNotes(changelog, '0.0.9', { repo, tag }), /is empty/)
})

test('the repository web URL comes out of the package manifest form', () => {
  assert.equal(repoWebUrl('git+https://github.com/Einsia/agent-git.git'), repo)
  assert.equal(repoWebUrl('https://github.com/Einsia/agent-git/'), repo)
})
