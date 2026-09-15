# Agit controller

This library owns outbound daemon peers and their RPC correlation state. It has
no dependency on the Agit CLI, harness drivers, native transcripts, repositories,
or settlement. The ordinary CLI composes it with its local executor; a cloud
service can depend on this library alone and supply its own authenticated API.

A host constructs `Controller` with a `Worker` executable and arguments. Calling
`connect` starts one supervised connection per peer ID. Repeated attachment with
unchanged configuration reuses that peer. Dropping a UI subscription does not
disconnect it; `disconnect` and dropping the controller do.

The current peer dialect is the owner RPC protocol (`machine.describe`, version
1). Hosts must authorize access before invoking this API. The SSH composition in
the CLI relies on the remote account and owner socket credentials. Generic cloud
caller delegation is an additional protocol requirement, not something granted
by constructing a WebSocket transport.

Requests use fresh wire IDs carrying a stable operation ID prefix. Responses and
events stay scoped to their peer; each successful handshake advances its
generation. Cached clients use `Status::target()` and `request_at` to fence
requests to that route and generation; an old attachment cannot address a new
configuration that reuses the peer ID. Per-peer counts and byte budgets bound pending work. A queued request
can expire before dispatch; a written request with no reply has an unknown
outcome. Expiration does not disconnect unrelated sessions. Mutations are never
automatically replayed. A fixed read-only method set may retry once after a new
connection generation becomes ready, within the original deadline.

`subscribe` returns a bounded event receiver. Consumers must treat lag as a gap
and reattach/replay from their own cursor. The controller preserves remote stream
IDs inside an envelope containing the peer ID and generation; it does not merge
streams belonging to different machines.

Connection state and pending correlation are in memory. Durable operation
receipts, delegated grants, executor ownership handoff, and cloud relay routing
remain separate integration work tracked by the RFC.

```sh
cargo check -p agit-controller --no-default-features
cargo test -p agit-controller
```
