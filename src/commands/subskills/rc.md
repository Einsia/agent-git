---
name: agit-rc
description: Pair the local agitd daemon and observe or drive supervised sessions from the web interface.
---

# agit rc

## Concepts

`rc` manages the remote-control daemon (`agitd`). The “web interface” is the Hub-connected control UI: it can observe live sessions on paired machines and send permitted actions. It is not another Git repo for the project directory. Workspace bindings and web pairing are separate metadata.

## Subcommands

| Subcommand | Purpose | Arguments |
|---|---|---|
| `start` | Pair and start the daemon | `--detach`, `--name <name>` |
| `status` | Connection state, uptime, and live sessions | none |
| `stop` | Stop the daemon; its sessions end | none |
| `list` | List registered machines for the account | none |
| `revoke <connection>` | Revoke a machine immediately; it cannot auto-register again | connection id |
| `pair` | Print a new pairing code | none |

All subcommands support common global options; `--json` emits the unified CLI JSON envelope.

## Examples

```bash
agit rc start --name laptop --detach
agit rc status
agit rc pair
agit rc list
agit rc revoke conn_123
agit rc stop
```

With `AGIT_RC=1`, the daemon may commit/push at turn boundaries. That is supervisor behavior, not a normal CLI guarantee; shared-workspace approvals go to the workspace owner.

## Proxy connections

The daemon reads proxy settings when it connects or reconnects. `wss://` uses
`https_proxy` / `HTTPS_PROXY`; `ws://` uses `http_proxy` / `HTTP_PROXY`.
`all_proxy` / `ALL_PROXY` is the fallback. Uppercase variables take precedence.
`no_proxy` / `NO_PROXY` bypasses the proxy for matching domains, IP addresses,
IP subnets, or `*` (all hosts).

Proxy URLs must use `http://` for an HTTP CONNECT proxy; optional URL credentials
are sent only to the proxy. TLS still verifies the original hub hostname and
certificate inside the tunnel. HTTPS and SOCKS proxy URLs produce an explicit
unsupported-proxy error instead of silently falling back to a direct connection.

```bash
HTTPS_PROXY=http://proxy.example.com:8888 agit rc start --detach
```

DNS, TCP connection, CONNECT, TLS and WebSocket negotiation share a 30-second
deadline. Connection errors identify whether the attempt was direct or through
a proxy. Set the environment in the shell that starts the daemon; changing a
different shell does not change the environment of an already running process.
