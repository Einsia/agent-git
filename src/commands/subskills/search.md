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
| `--scope <mine\|org\|public\|owner/repo>` | Restrict sessions or agents before ranking, counts and pagination |
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
`--counts`. `--here` intersects these scopes for sessions; `--local` is not supported here.

Scope is applied to every effective query, including shared `--repo` and `--owner`
filters, before any search request. A contradictory qualifier rejects the entire
batch. `mine` obtains the current identity once for the batch. Each structured result
reports its scoped query; saved-author/time filters and pagination remain independent.

```bash
agit search "rate limit" --scope mine
agit search "rate limit" --scope org
agit search --query "cache" --query "deploy" --scope public
agit search "rate limit" --scope einsia/payments --author "Alice"
```

`--scope org` searches readable repositories owned by organizations you currently
belong to. Membership narrows the corpus; it does not grant private-repository
access. Existing query qualifiers and saved-author/time filters further intersect
this scope. Personal repositories and organizations you have not joined remain
outside it even when you can read them.

Every organization-scoped response must confirm `applied_scope: "org"` and the
requested result type. Missing or mismatched confirmation withholds the result;
a single query exits with precondition code 4, and a batch marks that query as an
error. The CLI never retries without the scope. Structured results preserve the
acknowledgement, filters, pagination and uncertainty. The local MCP search tool
forwards `scope: "org"` through the same command.

## Search this code repository

Use `agit search "query" --here` to restrict session results to the current code Git
repository after `-C`. This requires one explicit `remote.origin.url` from local Git
configuration, without includes. It then reads Git's effective `remote get-url origin`,
including user and local URL rewrite rules, to match the origin used when recording
code provenance. System Git configuration is excluded, as it is when recording.
Both the configured origin and the effective URL must be safe. It does not use
`AGIT_SESSION`, an Agent repo binding, another remote, or a local-search fallback.

The Hub compares that exact origin with the original `code` field in each saved
version's `session/meta.json`, before collapsing sessions, counting or paging.
Transport, SSH username, hostname spelling, port, path and `.git` suffix all remain
part of the identity. Equivalent-looking transport URLs do not match automatically.
The recorded code commit suffix can be Git's abbreviated or full hexadecimal ID;
absent or malformed provenance does not match. Failed or budget-limited reads produce incomplete results.

This option supports `--type sessions` only and rejects `--counts` before a request.
It can intersect `--scope org`, `mine`, `public`, or `owner/repo`; all repository ACLs
still apply. A Hub that cannot echo the exact `applied_filters.code_origin` causes
a single-query exit 4 before hits or counts are shown; a batch marks each unconfirmed query as an error. Authentication remains required.

HTTPS/HTTP/git/SSH URLs and SCP-style origins with explicit hosts and paths are
accepted conservatively. SSH usernames such as `git@host:team/repo.git` are retained.
Passwords, HTTP userinfo, query/fragment data, percent-escaped forms, local paths,
whitespace and ambiguous origins are refused without printing or sending the value.
Set a single credential-free origin explicitly and keep any URL rewrites credential-free.
The CLI uses the effective URL exactly; it does not normalize URLs or guess aliases.

`agit search --here` also supports a filter-only query without text; it still requires
login and a validated code origin. Explicit empty `--query` values are refused.

The origin is resolved once before dispatching a batch. Saved-author and time filters
intersect it, and every structured result includes the acknowledged `applied_filters`.
