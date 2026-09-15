# Agit tunnel

A tunnel worker establishes one connection and transports packets. It has no
knowledge of sessions, RPC methods, caller grants, or application retry policy.
The parent owns reconnect decisions and the worker's process lifetime.

The client library starts a worker over private stdin/stdout pipes. `Open`
provides a versioned transport configuration; `Connected` identifies the worker.
`Send` carries a serial number and packet. `Written` confirms only that the
provider accepted the write; it does not prove remote execution. `Received` and
`Failed` report transport facts. The protocol bounds records and queues, and a
partial input record survives cancellation of a read future.

The worker feature supplies WebSocket and SSH providers. WebSocket honors proxy
configuration, including HTTP CONNECT. SSH executes a caller-supplied argument
vector as a remote stdio endpoint and transports newline-delimited text records.
The host, rather than the SSH provider, chooses any Agit command in that vector.

The CLI embeds the worker entry point as `agit rc tunnel` in a separate process.
Hosts can also build the standalone executable:

```sh
cargo build -p agit-tunnel --features worker --bin agit-tunnel
cargo test -p agit-tunnel --features worker
```

Dropping both halves of a connection terminates its worker process tree. A worker
crash or congested connection fails that connection without taking down another
worker. Credentials travel in the private opening record, not process arguments.
