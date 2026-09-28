---
name: agit-mcp
description: Start the stdio MCP server that exposes readable AgentGit history to agents.
---

# agit mcp (hidden)

## Synopsis

```bash
agit mcp
```

## Options

The command supports global `-y/--yes`, `-q/--quiet`, `-C/--directory`, `--no-color`, `-h/--help`, and `-V/--version`. Global `--json` does not change the JSON-RPC stdio protocol: requests and responses travel over stdio and the runtime or MCP client owns the lifecycle.

## Scenario

After `agit setup --mcp`, Codex/Claude or another client can launch it. For interactive history search, prefer `agit search` or a configured MCP tool; do not treat the MCP server as an interactive CLI.

## Agent calls

The `search` tool accepts `query` and/or `queries`, plus shared `repo`, `owner`,
`runtime`, `scopes`, `tool`, `path`, `type`, `sort`, `page`, and `limit` fields.
Filter-only requests are valid. A batch contains up to 16 queries and runs at
most four requests concurrently; output order follows the input. Search results
retain paging, uncertainty, and evidence fields. See `search.md` for semantics.

The `commit` tool accepts `target` (`owner/repo@branch`) and `milestone`; a `null` or
empty `target` counts as omitted. Without `target`, a server whose environment
carries `AGIT_SESSION` settles that session. Otherwise the tool settles nothing: the
server's environment is fixed when it starts and can name an earlier conversation
than yours, so it returns an error naming the runtime session its environment names
(for example through `CODEBUDDY_SESSION_ID`) and that session's saved target. Compare
that ID with your own conversation's ID and, if they match, call `commit` again with
that `target`. An unsaved conversation, several named conversations, or no identity
at all is an error naming the next command; the tool never falls back to a directory
binding or the most recent session.

Tool failures set MCP `isError: true`; a failed search batch still carries its
successful entries. Non-search tools use the unified CLI JSON envelope, except
`view`, which returns the structured VIEW itself for compatibility. Child CLI
processes cannot read MCP stdin or open terminal pickers.
