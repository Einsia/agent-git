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

Tool failures set MCP `isError: true`; a failed search batch still carries its
successful entries. Non-search tools use the unified CLI JSON envelope, except
`view`, which returns the structured VIEW itself for compatibility. Child CLI
processes cannot read MCP stdin or open terminal pickers.
