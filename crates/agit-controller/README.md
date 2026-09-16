# Agit controller

This library owns outbound daemon peers and their RPC correlation state. It has
no dependency on the Agit CLI, harness drivers, native transcripts, repositories,
or settlement. The ordinary CLI composes it with its local executor; a cloud
service can depend on this library alone and supply its own authenticated API.

A host constructs `Controller` with a `Worker` executable and arguments. Calling
`connect` starts one supervised connection per peer ID. Repeated attachment with
unchanged configuration reuses that peer. Dropping a UI subscription does not
disconnect it; `disconnect` and dropping the controller do.

The peer handshake (`machine.describe`) carries the route authority. The SSH
composition relies on the remote account and owner socket credentials. Cloud
routes authenticate endpoints with peer TLS and preserve the connection grant
principal; the executor filters resources and operations for that principal.

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
receipts and session policy remain executor responsibilities. Cloud admission
and the opaque relay do not interpret those runtime records.

The optional `cloud` feature provides the shared authenticated Cloud connector.
The `host` feature builds `agitd-controller`, a private stdio process for Web
adapters. Initialization supplies the Cloud origin and browser account token over
stdin. A leased outbound identity is renewed while the host is running and revoked
on shutdown. The Cloud identity service expires abandoned leases. Tokens and
private keys never appear in arguments or diagnostic records.

The host accepts Cloud peer methods only, bounds pending requests and bytes, and
preserves structured executor errors and transport outcomes. A slow output or lost
event cursor ends the attachment so clients can reconnect and reconcile. Each
connection uses a separate tunnel worker process, running the same executable with
the `tunnel` argument. Neither process contains an executor or starts a harness.

```sh
cargo check -p agit-controller --no-default-features
cargo test -p agit-controller --features host
cargo build -p agit-controller --features host --bin agitd-controller
```

For read-only phase timings against an existing Cloud executor, build the optional
probe with `cargo build -p agit-controller --features host --example cloud_probe`.
Pass one JSON line on stdin containing `hub`, `account_token`, `device_id`,
`samples` (1–20), and `idle_seconds` (0–3600). Read credentials from a private file
or credential helper; do not place tokens in shell arguments or paste them into
logs. The probe creates a disposable outbound controller, measures discovery,
grant creation, worker/WebSocket establishment, relay pairing, endpoint TLS,
executor RPCs and idle retention, then revokes its controller identity. It does
not create sessions, send prompts or change executor files. Standard output is
JSON Lines with phase durations and connection IDs for matching relay logs;
response bodies and credentials are omitted. A failed cleanup requires checking
the temporary controller lease. This is an exploratory diagnostic, not a CI gate.

For a controlled routing comparison, optional `transport_origin` selects a trusted
relay endpoint while `hub` remains the public issuer. HTTPS, loopback, and cluster
service origins are supported. The library exposes the same separation through
`Client::with_trusted_transport_origin`; it changes HTTP and WebSocket routing,
preserves device and grant issuer checks, and bypasses environment proxies only
for the explicitly configured trusted transport. Do not derive this origin from
an executor response or browser request.

Trusted WebSocket routes carry an optional direct-transport flag to the bundled
worker. Default CLI routes omit it and retain environment-proxy behavior. Deploy
the host and its bundled worker together when enabling an internal route; the
executor protocol does not change.
