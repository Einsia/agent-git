# Publication and local privacy

Privacy processing is optional local work governed by
[Local Privacy](rfc-local-privacy.md). A failed or incomplete scan cannot reject
ordinary publication. `--allow-secrets` remains accepted for command-line
compatibility and is unnecessary for upload.

The client protects new conversation content when it can durably retain reverse
mappings, then publishes existing Git objects through ordinary authentication,
authorization and integrity checks. Each push independently schedules encrypted
dictionary synchronization. All LFS payloads are excluded from privacy work;
their normal identity, size and transfer checks remain.

The Hub neither scans nor redacts content and has no privacy acceptance gate.
Private-to-public visibility still requires the ordinary destination/ref-bound
confirmation. It does not require accepting secret findings.

`agit scan` remains an explicit diagnostic. `agit push --audit` remains an
explicit interactive model review and publication confirmation. It receives the
captured non-LFS review surface selected by the user and can report its own
unavailable/incomplete review; ordinary push does not invoke it.
