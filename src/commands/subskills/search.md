---
name: agit-search
description: Search readable AgentGit history with structured results, shared filters, and bounded batches.
---

# agit search

```bash
agit search "rate limit" --repo alice/service --in tool --json
agit search --query "cache failure" --query "build timeout" --owner alice --json
agit search --repo alice/service --runtime codex --page 2 --limit 20 --json
agit search --author alice@example.org --since 2026-09-01 --before 2026-10-01 --json
```

Use `query` for one search or repeat `-Q, --query` for a batch. A positional query
can be combined with additional `--query` values. Shared flags apply to every
query; filter-only queries are supported. Batches retain input order, allow up to
16 queries, and run at most four requests concurrently using one HTTP pool. A
failed query does not discard successful results; any failure makes the command
exit nonzero. Retry only failed entries. Each expanded query is limited to 256
characters, matching the Hub request budget.

| Option | Meaning |
|---|---|
| `--scope <mine\|public\|owner/repo>` | Restrict sessions or agents before ranking, counts and pagination |
| `--repo <owner/name>` | One Agent repo; equivalent to `repo:` / `agent:` |
| `--owner <name>` | Repo owner, including an organization; not the conversation author |
| `--author <name/email>` | Exact Git author name or email of the selected saved version, case insensitive |
| `--since <date/time>` | Saved committer time at or after a UTC date or RFC3339 timestamp |
| `--before <date/time>` | Saved committer time strictly before the boundary |
| `--runtime <runtime>` | Session runtime, such as `codex` or `claude-code` |
| `--in <scope>` | `prompt`, `reply`, `tool`, `output`, `edit`, or `summary`; repeat to include scopes |
| `--tool <name>` | Tool-name substring |
| `--path <fragment>` | File-edit path substring; quote paths containing spaces |
| `-t, --type <kind>` | `sessions` (default), `agents`, `prs`, or `people` |
| `-n, --limit <count>` | Hits per query, 1–100; default 10 |
| `--page <n>` | One-based result page; default 1 |
| `--sort <order>` | `best`, `recent`, or `turns` |
| `--counts` | Counts for all types; this also scans sessions |
| `--json` | Unified CLI envelope; structured search data is in `result.value` |
| `--mcp` | Raw structured JSON without the CLI envelope |

When stdout is redirected or piped, search emits raw structured JSON by default.
Use `--json` explicitly when parsing all commands through the common envelope.
Human terminal output remains a readable list; a batch always emits JSON.

The query language accepts quoted phrases, `-exclude`, and qualifiers such as
`turns:>20`, `category:research`, `state:open`, and `is:public`. Quote the whole
query in the shell. Use `--query '-deprecated'` for a query beginning with `-`.
Prefer `--repo` or `--owner` to narrow expensive scans. `--in`, `--runtime`,
`--tool`, `--path`, and `turns:` apply to session search; other types report
unsupported qualifiers in `unknown`.

A single structured result contains `query`, `type`, `total`, `page`, `per`,
`has_more`, `incomplete`, `unknown`, `terms`, `applied_filters`, and `hits`. A batch contains
`batch: true` and `results`, each with `query`, `ok`, and either `result` or
`error`. Hit fields from the Hub are preserved, including `timestamp`, `line`,
`scope`, `secondhand`, `outcome`, `confidence`, and `outcome_reason`.

Treat `incomplete: true` as a lower bound, and inspect `unknown` before trusting
a filter. A summary hit is secondhand; an outcome is a heuristic. Open the hit's
session and relevant event before reusing a solution. Visibility is enforced by
the Hub before counting; private content you cannot read is excluded.

`--author`, `--since`, and `--before` apply only to session searches. Author is
recorded Git metadata, not an authenticated Hub account and not the repo owner.
Time means when the selected version was last saved (Git committer time), not
when a matched transcript event occurred. Date-only boundaries mean midnight UTC;
RFC3339 offsets are normalized to UTC. The start is inclusive and the end is
exclusive. Use the session result's `total` for a filtered count; these flags
cannot be combined with the multi-category `--counts` option.

These options use dedicated HTTP parameters rather than query qualifiers. Do not
write `author:`, `since:`, or `before:` inside the query. `applied_filters` echoes
the normalized predicates; an older Hub that omits them causes an explicit error
instead of an unfiltered answer. Metadata is checked before blob deduplication,
ranking, counts and pagination, under the same repository permissions as content.
Missing metadata produces `incomplete: true` and never relaxes the filters.

## Repository scope

Every remote search requires login, including public search. `--scope mine` selects
repositories in the current authenticated account's personal namespace; it does not
include repositories shared through collaboration or organization membership.
`--scope public` selects public repositories; `--scope owner/repo` selects that exact
repository. These scopes apply to sessions and agents, and cannot be combined with
`--counts`. Other repository scopes, `--here`, and `--local` are not supported here.

Scope is applied to every effective query, including shared `--repo` and `--owner`
filters, before any search request. A contradictory qualifier rejects the entire
batch. `mine` obtains the current identity once for the batch. Each structured result
reports its scoped query; saved-author/time filters and pagination remain independent.

```bash
agit search "rate limit" --scope mine
agit search --query "cache" --query "deploy" --scope public
agit search "rate limit" --scope einsia/payments --author "Alice"
```
