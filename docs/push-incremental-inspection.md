# Incremental push inspection

Push retains a complete frozen publication plan for source verification and literal
upload refspecs. A separate inspection scope selects new commit bodies, annotated
tag bodies, blobs and LFS pointers. Manual publication, automatic publication and
`--dry-run` use the same selection. `--audit` selects complete history and payloads.

The baseline comes from a fresh authenticated advertisement of the selected URL,
bound to the immutable repository identity. Local tracking refs and saved publication
receipts do not establish object presence. An empty or unavailable advertisement,
an invalid baseline, or first publication produces full inspection. Advertised roots
unavailable locally contribute no exclusions. Enumeration uses isolated Git storage
so shallow boundaries, grafts and replacement refs cannot hide history.

Encrypted publication computes the scope after projection. Original source IDs cannot
exclude public projection objects. Preparation retains integrity and binding checks;
policy and recipient changes that produce new objects enter inspection. New tags
targeting old commits are inspected independently of commit selection. Every unpublished
snapshot participates, including files deleted before the current tip.

Only pointer blobs in the Git inspection scope select LFS payloads. Selected payloads
still require length and hash verification and readable-content inspection, including
when the destination reports the payload as present. Missing or corrupt required data
blocks publication. A repeated push does not require old payloads merely to inspect
already advertised history. URL, repository identity and frozen source state are
rechecked before publication.

Updated scanner rules do not automatically recheck remote history during incremental
push. Use `--audit` for complete review. No scan-result cache is retained across pushes,
and server-side receive policy is unchanged.

## Local measurement

The filesystem fixture prints separate history-enumeration, scope, payload-staging
and content-scan timings. The projection fixture reports preparation and reuse:

```sh
cargo test --lib incremental_scope_preserves_history_tags_and_payload_boundaries -- --nocapture
cargo test --lib incremental_reuse_rechecks_mutable_dependencies_and_authenticates_its_index -- --nocapture
```

A September 29, 2026 local debug-build run produced these observations. Other checks
ran concurrently; these are fixture observations, not throughput claims. Git object
counts include trees. Payload bytes are the selected inventory per staging or
inspection pass, rather than operating-system I/O counters. Network advertisement,
upload and extra object-count enumeration are outside the timed phases.

| Ordinary scenario | Selected Git objects | Selected payload bytes | History | Scope | Staging | Content scan |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| First publication | 4 | 18 | 20.930 ms | 89.533 ms | 2.733 ms | 411.073 ms |
| Repeated publication | 0 | 0 | 18.044 ms | 58.163 ms | 0.405 ms | 0.006 ms |
| Appended merge history | 8 | 0 | 28.411 ms | 51.116 ms | 0.167 ms | 127.519 ms |

The projection fixture separately observed 363.596 ms for initial preparation and
213.650 ms for reuse. It checks that reused public IDs yield an empty scope, original
source IDs do not exclude projection history, and changed rules or recipients produce
objects selected for inspection.

Full source-history enumeration and baseline graph enumeration remain costs of push.
Projection preparation also retains validation work. This change reduces content
inspection and historical payload reads; it does not make every push phase incremental.

The HTTP regression test additionally exercises live advertisements and binding:

```sh
cargo test --lib incremental_capture_uses_live_refs_and_keeps_new_pointer_validation -- --nocapture
```

The filesystem fixture, publication-plan, scanner, LFS history, frozen transport,
and push decision tests passed in the verification sandbox. The operator also reported
the HTTP regression passing in a local terminal outside the sandbox.
