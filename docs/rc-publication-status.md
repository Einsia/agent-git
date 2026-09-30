# Native capture and publication status

Native takeover resolves the exact runtime/native session link on the executor. A valid
link retains its repository, branch, workspace, baseline and recovery metadata. A missing
link permits an unbound conversation; malformed, conflicting, superseded and mismatched
claims refuse managed takeover. Project names and browser parameters cannot select a
publication destination.

The roster persists capture kind separately from controller authority:

| Capture kind | Local save | Publication destination |
| --- | --- | --- |
| `hub_linked` | Existing imported repository and branch, pinned to its immutable Hub identity | That repository's verified Hub slug and identity |
| `device_local` | Device repository and branch, pinned to machine authority | The separately confirmed `agit.desktopPublication` destination |

Capture is persisted before native launch. A restarted catalog exposes dormant logical/native
associations without starting a writer. Resume revalidates the native claim and retained kind;
duplicate native/logical requests retain existing launch reservations and writer arbitration.
Local saving does not depend on publication consent.

## Additive wire contract

Executors advertise `session-publication-status-v1` in `rpc_features`. `SessionInfo`,
`session.publication.changed` parameters and `session.publication.deliver` results may contain:

```json
{
  "publication": {
    "readiness": "setup_required",
    "progress": "local_saved",
    "stage": "consent",
    "reason": "consent_required"
  }
}
```

| Field | Values |
| --- | --- |
| `readiness` | `unbound`, `checking`, `setup_required`, `ready` |
| `progress` | `idle`, `local_saved`, `publishing`, `awaiting_ack`, `acknowledged`, `failed` |
| Optional `stage` | `capture`, `configuration`, `consent`, `publication`, `receiver` |
| Optional `reason` | `unbound`, `binding_invalid`, `push_disabled`, `destination_missing`, `consent_required`, `eligibility_unchecked`, `publication_pending`, `publication_failed`, `receiver_unavailable`, `receiver_rejected` |

Readiness and progress are independent. Start/resume/list/watch snapshots invalidate cached
authorization to `checking` for a bound session, or report `unbound`; they retain available
progress. Publication hints update progress under the existing stream/generation fences.
Delivery checks the current canonical Hub, immutable destination, URL, account and write access.
An encrypted destination also checks visibility, effective privacy policy and viewing recipient
against explicit push consent; an ordinary destination needs no saved consent.
Readiness is never persisted as authorization.

An unbound delivery keeps RPC `201` with `error.data.publication`. Capture admission errors
may carry the same field with `binding_invalid`. Missing configuration or consent returns an
empty delivery page, a readiness status, and no retry timer; it does not call the receiver.
Unavailable eligibility remains `checking`. Local state and transport failures retain existing
numeric error behavior. Only an attempted receiver confirmation can report `receiver_rejected`.
Consumers must use notification/receipt identity and current coverage to confirm a visible turn;
the progress label alone cannot prove coverage.

## Consumer behavior

Consumers negotiate the capability and allowlist only the bounded enum fields above in error
data. No paths, conversation text, repository credentials or private commit IDs belong in this
DTO. Preserve incarnation, generation and sequence checks on both results and notifications.
After reconnect or a capture/configuration change, invalidate cached eligibility and request a
fresh delivery scan. Stop automatic retry when setup is missing; explicit refresh or a new
publication hint can trigger another check. A peer without the capability retains legacy
behavior: numeric `201` alone means delivery is unavailable, not receiver rejection.

The adjacent backend's conservative legacy consumer and authenticated browser-shaped takeover
acceptance work with this additive contract. Rendering each new enum requires the consumer
integration described in `local-handoff/privacy-rc-takeover-publication-status.md`.

## Acceptance

CLI regressions cover exact-link resolution, both repository kinds, conflict and identity drift,
an existing unbound roster, failed persistence, dormant restart discovery, consent changes and
durable acknowledgement replay. The adjacent backend opt-in test
`native_takeover_push_reaches_durable_publication_receipt` starts without roster lineage and
covers terminal import, explicit protected push, browser-shaped takeover, a new native turn,
local save, encrypted Git publication, receiver ACK and restart replay. Its workspace project
differs from the imported publication target. The bound device-local control is
`live_supervisor_push_reaches_durable_publication_receipt`.
