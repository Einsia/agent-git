---
name: agit-activity
description: Query saved conversation messages by person and time with automatic local or Hub routing.
---

# Query conversation activity

Use `agit activity` to read what a person saved to an AgentGit repository during a time range. No keyword is required. Prefer a Hub username or account ID for `--author`; exact Git author names and emails remain available for original or legacy plain commits.

```bash
agit activity alice/research --author alice --since 2026-10-01T00:00:00+08:00 --before 2026-10-02T00:00:00+08:00 --json
agit activity alice/research --author alice --since 2026-10-01T00:00:00+08:00 --before 2026-10-02T00:00:00+08:00 --session experiment --limit 100 --json
agit activity alice/research --author alice --since 2026-10-01T00:00:00+08:00 --before 2026-10-02T00:00:00+08:00 --assistant last --json
```

`--assistant all` (the default) returns every user and assistant message. `--assistant last` returns every user message but only the last assistant message before each next user message or the end of a contribution: the answer to each request, without the progress commentary an agent writes between tool calls. Use `last` to summarize what someone asked for and what they got; use `all` when the intermediate reasoning matters.

If the repository exists locally, the first page fetches its latest remote branches and queries local objects, including unpushed local commits; continuation pages read the same local snapshot without fetching again. Fetch does not move local branches or check out files, and a failed fetch fails the query. Without a local repository, the command queries the Hub directly and does not clone it. JSON reports `source` as `local` or `hub`. Local content can include private originals and unpublished work; Hub content is the published projection. A fetched encrypted projection uses Hub publication records even on the local route; it does not decrypt originals.

`--since` is inclusive and `--before` exclusive, and both apply to when each message was sent. Use RFC3339 with a timezone for local days; date-only values mean midnight UTC. Each message has `sent_at`: the time Claude Code, Codex or OpenCode recorded in its own record (`sent_basis: recorded`), or its contribution's save time when the record has none (`sent_basis: saved`). A message saved or pushed days later still counts on the day it was sent. Each contribution also has `saved_at` and a `time_basis`: original/plain commits use Git committer time (`committed_at`), while encrypted publications use the Hub's acceptance time (`published_at`) and are selected by that time. This command does not infer the original human speaker or use the namespace owner as the author.

Encrypted publication identity comes from accepted Hub records, independent of the anonymous Git headers. `author_account_id` identifies the saving account. `--session` accepts a branch name or an exact logical or published AgentGit session ID; encrypted contributions report the logical session ID. Local originals take precedence when their published copy is also present.

`contributions` lists one entry per saved commit, oldest first, with `session`, `commit`, `turn`, `saved_at`, `time_basis` and author fields. Its `messages` hold only newly appended user prompts, user interjections and assistant replies, each with `role`, source `line`, `sent_at`, `sent_basis` and `text`. Tool calls/results, thinking, summaries and inherited messages from other sessions are omitted, and older cumulative content is not repeated. Contributions with the same save time are ordered by session and turn, so a batch import or publication keeps conversation order.

A page holds at most 50 message chunks by default; `--limit` accepts 1-200. A message longer than the page's text budget is split: chunks after the first carry `text_offset` (bytes), and every chunk but the last has `text_complete: false`. The same commit then continues in the next page's first contribution. Continue with the same arguments plus `--cursor <next_cursor>` until `has_more` is false, including after empty pages. A cursor resumes after the last returned position: saves that land later extend the result, and a save pushed later with an earlier save time appears only in a fresh query. Contributions stay in save order, so sort messages by `sent_at` for a timeline. A cursor cannot move between a local checkout and the Hub, or between filters.

`incomplete` covers this page and every earlier page of the query. `reasons` maps each cause to a `count` for the whole query and a few example commits from the page that found them: `missing_publication_metadata` and `unmapped_publication_author` (encrypted work without usable attribution), `unreadable_commit`, `publication_mismatch`, `history_truncated` and `changed_during_pagination`. Never describe an incomplete result as exhaustive absence. `warnings` such as `author_not_found` (no account or Git author matches) do not affect completeness. `--format markdown` produces readable text. Both routes require a Hub supporting activity schema version 3.
