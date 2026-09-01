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
