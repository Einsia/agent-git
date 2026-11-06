# Privacy benchmark case catalog

This catalog covers every checked-in case. Inputs and annotations are in
[the corpus](../benchmarks/privacy/cases/synthetic.json) and
[the annotations](../benchmarks/privacy/annotations/synthetic.json).

Scenario expectations are selected from credential semantics, explicit user rules,
publication contracts and public format specifications before a tested CLI runs.
A detector miss does not authorize weakening an expectation. A malformed fixture
is corrected against its format specification, with the reason documented separately.

All values are synthetic and endpoints use reserved example domains. Provider-shaped
values are not issued by a service. The signing key pair was generated solely for
this fixture, is public test material, and must not secure any deployment.
The JWT has a verifiable test signature; the PNG has valid chunks, checksums and pixels.

Run `python3 benchmarks/privacy/validate_fixtures.py --external` to check declared
Python, JavaScript, shell, INI, JSON, HTTP, URI, JWT, PEM and PNG specimens. The
external checks require Node.js, OpenSSL and a POSIX shell and only parse or inspect
the fixtures. Without `--external`, required external checks are listed as deferred.
These checks are independent of the privacy scanner and never contact a credential
issuer. Workflow validation replays the fixed calculator edit and runs its local
check script in an isolated temporary project; it does not execute recorded shell
commands or start an agent. Native transcript fields and linkage are validated
by the benchmark input contracts. Natural-language prose has no formal syntax check.

Existing boundary probes remain useful alongside the realistic scenario fixtures.
In particular, `private-key` is a synthetic region-recognition probe rather than a
complete PEM key, and the original `attachment-*` probes use encoded stand-in bytes.
The scenario key-pair and image cases add fully valid artifacts for those workflows.

The `native-*` cases are protocol-contract probes with synthetic Read/ReadPublic tools.
The `workflow-*` cases use separately documented tool profiles and a read/edit/test
sequence. They are constructed traces, not recordings of live agent execution.
See [workflow profiles and limits](privacy-benchmark-workflows.md).

The `incident-*` cases reconstruct reviewed reports with new synthetic values.
Their evidence notes distinguish directly observed anomalies from workflow
corroboration and report-only facts. See [incident triage](privacy-benchmark-incidents.md).

Regenerate this catalog with `python3 benchmarks/privacy/generate.py`.

## provider-repeated

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Each repeated provider credential is protected without deleting its neighbors.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## multiple-credentials

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Missing either credential counts as its own false negative.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## context-and-references

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** References remain readable beside a real password assignment.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## structured-fields

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Decoded credentials are protected and document tokens, types and escapes survive.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## protocol-id

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Native protocol identifiers keep identity while ordinary content is protected.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## normal-placeholders

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Document placeholders and configuration references are not actual credentials.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## bare-entropy-default

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: entropy, default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Bare entropy remains readable without opt-in.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## bare-entropy-opt-in

- **Coverage kind:** boundary probe.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Entropy opt-in protects a bare synthetic high-entropy value.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## explicit-block

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** An explicit rule protects an otherwise ordinary value.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## allow-default

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** An explicit allow overrides heuristic detection.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## block-over-allow

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** An explicit block wins when the same value is allowed.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## url-password

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** A URL credential is protected without removing the host and path.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## private-key

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** The complete synthetic private-key block is protected.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## default-file-exclusion

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** File access policy follows the call through to its result; public source content stays readable.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## user-file-exclusion

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** File access policy follows the call through to its result; public source content stays readable.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## ordinary-file

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** File access policy follows the call through to its result; public source content stays readable.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## path-mention

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Mentioning an excluded path aliases the path and preserves the complete surrounding message.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## literal-replacement

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Literal substitution preserves adjacent conversation.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## regex-replacement

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Regex substitution changes only the selected content.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## attachment-omit

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Attachment mode controls the synthetic payload and retains the required notice or metadata.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## attachment-metadata

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Attachment mode controls the synthetic payload and retains the required notice or metadata.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## attachment-original

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Attachment mode controls the synthetic payload and retains the required notice or metadata.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## metadata-minimal

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Metadata mode limits project coordinates and never exposes the original device path.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## metadata-project

- **Coverage kind:** boundary probe.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Metadata mode limits project coordinates and never exposes the original device path.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## split-context

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** A credential whose assignment precedes the chunk retains its context.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## stream-boundary

- **Coverage kind:** boundary probe.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** A credential split across input chunks cannot escape through the released prefix.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## entropy-source-identifiers

- **Coverage kind:** boundary probe.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Typed digests and resource URLs retain their source evidence under entropy opt-in.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## plain-file-content

- **Coverage kind:** boundary probe.
- **Scope:** policy; plain; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Plain capture protection does not apply public-view file exclusions.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## privacy-export-file

- **Coverage kind:** boundary probe.
- **Scope:** policy; privacy_export; runtime: claude-code; profiles: default, all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Privacy export shares the frozen projection used by the encrypted public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## memory-allowlist

- **Coverage kind:** boundary probe.
- **Scope:** policy; memory; runtime: not runtime-specific; profiles: all.
- **Scenario:** A focused synthetic boundary probe for the behavior described below.
- **Expected behavior:** Memory is opt-in and sensitive paths remain excluded even with an allowlist.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-claude-code-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A claude-code protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-claude-code-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: claude-code; profiles: default, all.
- **Scenario:** A claude-code protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-claude-desktop-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: claude-desktop; profiles: default, all.
- **Scenario:** A claude-desktop protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-claude-desktop-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: claude-desktop; profiles: default, all.
- **Scenario:** A claude-desktop protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-cursor-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: cursor; profiles: default, all.
- **Scenario:** A cursor protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-cursor-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: cursor; profiles: default, all.
- **Scenario:** A cursor protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-codex-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: codex; profiles: default, all.
- **Scenario:** A codex protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-codex-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: codex; profiles: default, all.
- **Scenario:** A codex protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-opencode-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: opencode; profiles: default, all.
- **Scenario:** A opencode protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-opencode-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: opencode; profiles: default, all.
- **Scenario:** A opencode protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-hermes-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: hermes; profiles: default, all.
- **Scenario:** A hermes protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-hermes-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: hermes; profiles: default, all.
- **Scenario:** A hermes protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-openclaw-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: openclaw; profiles: default, all.
- **Scenario:** A openclaw protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-openclaw-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: openclaw; profiles: default, all.
- **Scenario:** A openclaw protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-workbuddy-encrypted-public

- **Coverage kind:** protocol-contract.
- **Scope:** policy; encrypted_public; runtime: workbuddy; profiles: default, all.
- **Scenario:** A workbuddy protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## native-workbuddy-plain

- **Coverage kind:** protocol-contract.
- **Scope:** policy; plain; runtime: workbuddy; profiles: default, all.
- **Scenario:** A workbuddy protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset.
- **Expected behavior:** Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## scenario-python-settings-review

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A user pastes a complete Python settings module while reviewing an accidentally hardcoded database password.
- **Expected behavior:** Protect the password literal; retain annotations, environment lookups, host and timeout.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** python.

## scenario-python-provider-references

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A user asks about a Python factory and variables that refer to a provider class, rather than stored credentials.
- **Expected behavior:** Preserve the valid class, constructor call and class reference, including the version digit in its name.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** python.

## scenario-javascript-client-options

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A Node.js client configuration is pasted into an assistant chat to debug authentication.
- **Expected behavior:** Protect the literal password without changing the environment reference, object syntax or timeout.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** javascript.

## scenario-ini-database-passphrase

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** An application operator pastes its INI database section while troubleshooting a connection.
- **Expected behavior:** Protect a human-readable passphrase even though it is less random than an API key; keep the remaining INI fields.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** ini.

## scenario-shell-password-value

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A startup script sets an actual password to a word also used as a class name in source code.
- **Expected behavior:** Protect the assigned password in this executable shell script; spelling alone does not make it a source reference.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** shell.

## scenario-curl-environment-references

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A README provides a runnable shell command that obtains credentials from the caller environment.
- **Expected behavior:** Keep variable references and the command intact; no credential value is present in the pasted script.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** shell.

## scenario-oauth-and-pagination

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A gateway debug response includes an authentication session and a pagination cursor; both happen to use the same opaque string.
- **Expected behavior:** Protect the access token and retain the pagination cursor, empty collection, boolean and expiry.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** json-value.

## scenario-http-basic-authentication

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** An HTTP diagnostic trace exposes a Basic Authorization header for a reporting endpoint.
- **Expected behavior:** Protect the complete encoded username/password credential, preserving the scheme and other request fields.
- **Expectation basis:** [rfc7617](https://www.rfc-editor.org/rfc/rfc7617).
- **Validity checks:** http-basic.

## scenario-http-signed-jwt

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A developer shares a gateway request trace containing a signed service JWT and an unrelated request identifier.
- **Expected behavior:** Protect the complete bearer token, including its signature; preserve the request identifier and HTTP syntax.
- **Expectation basis:** [rfc7519](https://www.rfc-editor.org/rfc/rfc7519).
- **Validity checks:** http-jwt.

## scenario-postgresql-encoded-password

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A database connection URI is copied from a migration error into the conversation.
- **Expected behavior:** Protect the percent-encoded password while retaining the username, host, port, database and query.
- **Expectation basis:** [libpq-connect.html#LIBPQ-CONNSTRING-URIS](https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNSTRING-URIS).
- **Validity checks:** postgresql-uri.

## scenario-gitlab-api-command

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A developer pastes the curl command used to diagnose a GitLab API permission failure.
- **Expected behavior:** Protect the synthetic GitLab personal token; retain the header name, endpoint and command structure.
- **Expectation basis:** [tokens](https://docs.gitlab.com/security/tokens/).
- **Validity checks:** shell.

## scenario-aws-shared-credentials

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A user shares a named AWS credentials profile after selecting the wrong account for a deployment.
- **Expected behavior:** Protect the secret access key; retain the profile name, non-secret access key identifier and INI layout.
- **Expectation basis:** [cli-configure-files.html](https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-files.html), [troubleshoot.html](https://docs.aws.amazon.com/us_en/IAM/latest/UserGuide/troubleshoot.html).
- **Validity checks:** aws-ini.

## scenario-signing-key-pair

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A service operator pastes a public/private signing-key pair to diagnose a key-format mismatch.
- **Expected behavior:** Protect the entire valid private PEM block, including its delimiters; retain the matching public key.
- **Expectation basis:** [rfc7468](https://www.rfc-editor.org/rfc/rfc7468).
- **Validity checks:** pem-pair.

## scenario-json-escaped-password

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A JSON diagnostics payload contains a password with a quote and a backslash, plus an escaped error message.
- **Expected behavior:** Protect the complete decoded password while retaining escapes, neighboring text and scalar types.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** json-value.

## scenario-unicode-authentication-log

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A localized authentication log is pasted with CJK text, an emoji and an accented service label.
- **Expected behavior:** Protect the password at the correct character and byte coordinates without damaging surrounding Unicode.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## scenario-stream-password-at-eof

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A terminal result arrives incrementally and ends directly after a quoted password assignment, without a newline.
- **Expected behavior:** Protect a credential split across chunks and the final flush; retain the valid shell assignment.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** shell.

## scenario-stream-multiple-credentials

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** An assistant receives a startup script in chunks that split both a password and a provider token.
- **Expected behavior:** Protect every occurrence across successive chunks while preserving complete public lines and recovery.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** shell.

## scenario-overlapping-user-secrets

- **Coverage kind:** workflow scenario.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A team explicitly protects both its current deployment code and the shorter code used by the previous deployment.
- **Expected behavior:** Protect the complete longer value and the separate shorter value; an overlapping block must not expose a suffix.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** json-value.

## scenario-files-and-secret-protection

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** The assistant reads a sensitive environment file and a public Python file containing an accidentally committed token; results return in reverse call order.
- **Expected behavior:** Hide the environment result even though it has no credential; retain the Python file with only its token protected and preserve call associations.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** python, shell.

## scenario-relative-paths-and-cwd

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A conversation runs in the source subdirectory and reads a parent environment file and a sibling JSON configuration.
- **Expected behavior:** Resolve relative paths from the captured working directory; exclude the environment file and retain the ordinary configuration.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** json.

## scenario-tool-omit-with-docs-exception

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** A repository omits local tools from public views while explicitly allowing a documentation-search MCP tool.
- **Expected behavior:** Apply the tool exception to both the allowed call and result, without exposing the omitted local result.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## scenario-image-metadata-and-caption-secret

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** An icon-upload bug report includes a valid PNG and a pasted bearer header in its caption.
- **Expected behavior:** Retain attachment metadata and the useful caption, omit image bytes, and protect the caption credential.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** png-base64.

## scenario-codex-terminal-secret

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: codex; profiles: default, all.
- **Scenario:** A Codex terminal call prints an environment variable while diagnosing a deployment authentication failure.
- **Expected behavior:** Keep the command and tool association, replace the local workdir with a documented public alias, and protect the printed credential; alias allocation is not a credential decision.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** json.

## scenario-branch-checklist-release

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** The same internal checklist is captured on a release branch and a documentation-review branch.
- **Expected behavior:** Apply the additional release-branch exclusion only on release; preserve the same file on the feature branch.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## scenario-branch-checklist-feature

- **Coverage kind:** workflow scenario.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** The same internal checklist is captured on a release branch and a documentation-review branch.
- **Expected behavior:** Apply the additional release-branch exclusion only on release; preserve the same file on the feature branch.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## workflow-claude-code-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: claude-code; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Claude Code Read / Edit / Bash.
- **Tool-contract sources:** [cc-2.1.311-trailing-records.jsonl](../tests/fixtures/cc-2.1.311-trailing-records.jsonl), [tools-reference](https://code.claude.com/docs/en/tools-reference).
- **Validity checks:** agent-workflow.

## workflow-claude-code-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: claude-code; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Claude Code Read / Edit / Bash.
- **Tool-contract sources:** [cc-2.1.311-trailing-records.jsonl](../tests/fixtures/cc-2.1.311-trailing-records.jsonl), [tools-reference](https://code.claude.com/docs/en/tools-reference).
- **Validity checks:** agent-workflow.

## workflow-claude-desktop-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: claude-desktop; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Claude Desktop with Desktop Commander MCP.
- **Tool-contract sources:** [schemas.ts](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts).
- **Validity checks:** agent-workflow.

## workflow-claude-desktop-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: claude-desktop; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Claude Desktop with Desktop Commander MCP.
- **Tool-contract sources:** [schemas.ts](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts).
- **Validity checks:** agent-workflow.

## workflow-cursor-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: cursor; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Cursor Read / ApplyPatch / Shell, checked-in transcript conventions.
- **Tool-contract sources:** [cursor.rs](../src/adapter/cursor.rs), [tools](https://docs.cursor.com/en/agent/tools).
- **Validity checks:** agent-workflow.

## workflow-cursor-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: cursor; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Cursor Read / ApplyPatch / Shell, checked-in transcript conventions.
- **Tool-contract sources:** [cursor.rs](../src/adapter/cursor.rs), [tools](https://docs.cursor.com/en/agent/tools).
- **Validity checks:** agent-workflow.

## workflow-codex-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: codex; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Codex exec_command / freeform apply_patch.
- **Tool-contract sources:** [codex.rs](../src/adapter/codex.rs).
- **Validity checks:** agent-workflow.

## workflow-codex-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: codex; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Codex exec_command / freeform apply_patch.
- **Tool-contract sources:** [codex.rs](../src/adapter/codex.rs).
- **Validity checks:** agent-workflow.

## workflow-opencode-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: opencode; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** OpenCode V2 read / edit / shell.
- **Tool-contract sources:** [tools](https://dev.opencode.ai/v2/docs/tools/).
- **Validity checks:** agent-workflow.

## workflow-opencode-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: opencode; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** OpenCode V2 read / edit / shell.
- **Tool-contract sources:** [tools](https://dev.opencode.ai/v2/docs/tools/).
- **Validity checks:** agent-workflow.

## workflow-hermes-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: hermes; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Hermes file and terminal toolsets.
- **Tool-contract sources:** [code-execution](https://hermes-agent.nousresearch.com/docs/user-guide/features/code-execution).
- **Validity checks:** agent-workflow.

## workflow-hermes-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: hermes; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** Hermes file and terminal toolsets.
- **Tool-contract sources:** [code-execution](https://hermes-agent.nousresearch.com/docs/user-guide/features/code-execution).
- **Validity checks:** agent-workflow.

## workflow-openclaw-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: openclaw; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** OpenClaw read / Pi batched edit / exec.
- **Tool-contract sources:** [agent](https://docs.openclaw.ai/concepts/agent), [exec](https://docs.openclaw.ai/tools/exec), [edit.ts](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/tools/edit.ts).
- **Validity checks:** agent-workflow.

## workflow-openclaw-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: openclaw; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** OpenClaw read / Pi batched edit / exec.
- **Tool-contract sources:** [agent](https://docs.openclaw.ai/concepts/agent), [exec](https://docs.openclaw.ai/tools/exec), [edit.ts](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/tools/edit.ts).
- **Validity checks:** agent-workflow.

## workflow-workbuddy-calculator-encrypted-public

- **Coverage kind:** tool-workflow.
- **Scope:** policy; encrypted_public; runtime: workbuddy; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** WorkBuddy with Desktop Commander MCP.
- **Tool-contract sources:** [MCP-Guide](https://www.codebuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/MCP-Guide), [schemas.ts](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts).
- **Validity checks:** agent-workflow.

## workflow-workbuddy-calculator-plain

- **Coverage kind:** tool-workflow.
- **Scope:** policy; plain; runtime: workbuddy; profiles: default, all.
- **Scenario:** The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.
- **Expected behavior:** Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.
- **Expectation basis:** [privacy-benchmark.md](privacy-benchmark.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Tool profile:** WorkBuddy with Desktop Commander MCP.
- **Tool-contract sources:** [MCP-Guide](https://www.codebuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/MCP-Guide), [schemas.ts](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts).
- **Validity checks:** agent-workflow.

## incident-stdlib-import

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** A local preview server is saved alongside a deployment credential while entropy replacement is enabled.
- **Expected behavior:** Retain the standard-library import, class inheritance and source references; protect the adjacent provider credential.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #66](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/66), [Frozen Hub session](https://agent-git.com/@leopierce/night-owl-index/s/agit-f7d113cee8eb65f2e207e8365799680fce623122?ref=95f9834d4d00a0818957397562a34cddf68900bb).
- **Evidence boundary:** The frozen Hub trace contains SimpleHTTPRequestHandler in an import and a Handler subclass. Issue 66 establishes the separate file-commit corruption; the current view is workflow corroboration.
- **Validity checks:** python.

## incident-relative-artifact-links

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** An agent writes a deliverable index whose relative PDF links must remain usable by another reader.
- **Expected behavior:** Keep both Markdown destinations and labels while protecting the adjacent credential. This checks text classification, not file-commit or link-resolution integration.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #67](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/67), [Frozen Hub session](https://agent-git.com/@jonahreid/weather-poetry/s/agit-a2ee913d7202709cd1acb9803e580992d34b4a7d?ref=942d3a2713f4f5efa511ec3f98472da2e96f4aae).
- **Evidence boundary:** The frozen Hub trace contains the reported relative PDF deliverable path and README handoff. The issue documents corruption during a later local file commit, not successful publication of a broken README.
- **Validity checks:** markdown-links.

## incident-ppt-enum-references

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** An agent stores a Python presentation generator using dotted enum constants and a ZIP compression constant.
- **Expected behavior:** Preserve executable enum references and module names under entropy opt-in; protect the credential literal independently.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69), [CLI issue #66](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/66), [Frozen Hub session](https://agent-git.com/@einsia/ppt-bench/s/agit-f2ef05924581003865ddaeea71ccbb1c8a53e55b?ref=11b3295de8de76d70752129783626f02a3a7ca87).
- **Evidence boundary:** The frozen presentation session contains MSO_ANCHOR.MIDDLE and MSO_SHAPE references in generated Python. The bulk false-positive counts come from issue 69, not a count inferred from this rendered view.
- **Validity checks:** python.

## incident-project-key-without-marker

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** A user pastes a provider-shaped project key while debugging a rejected request; the body has no embedded vendor marker.
- **Expected behavior:** Protect the complete credential-shaped value by default and retain the question and punctuation. A rejected or synthetic key still must not teach the detector to expose that shape.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #114](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/114), [CLI issue #136](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/136).
- **Evidence boundary:** These reports explicitly used generated fake keys. The replacement is also synthetic, not asserted to be an issued or working provider key.
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## incident-beta-flags-and-usage

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; default detection; runtime: not runtime-specific; profiles: default, all.
- **Scenario:** An agent inspects request feature flags and token-usage counters beside an Authorization header.
- **Expected behavior:** Keep feature flag names, usage keys and numeric values; protect only the credential in the header.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #99](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/99), [CLI issue #69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69).
- **Evidence boundary:** The issue reports the failure; this case reconstructs its local protection boundary.
- **Validity checks:** json-value.

## incident-public-reference-urls

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** An agent cites a public data URL and a separate authenticated endpoint in a research handoff.
- **Expected behavior:** Retain the public URL and ordinary query parameters under entropy opt-in; protect the credential-bearing query value without exempting all URLs.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69), [Frozen Hub session](https://agent-git.com/@ethanm/coffee-evidence-map/s/agit-758354fc4907620c6ef496cf60810072cac22d64?ref=e173cc9aeb00746ecc558f8d4e957b3e0f45a25c).
- **Evidence boundary:** The frozen public session visibly contains V1 placeholders in call_id fields and strings shaped as https: followed by a placeholder. Original hidden URL bytes are unavailable; the URLs here are newly constructed public-reference examples.
- **Validity checks:** reference-urls.

## incident-claude-native-identities

- **Coverage kind:** incident-reconstruction.
- **Scope:** policy; plain; runtime: claude-code; profiles: entropy, all.
- **Scenario:** A native Claude call/result pair carries UUID linkage fields, while the same UUID is explicitly blocked as ordinary command/result content.
- **Expected behavior:** Preserve verified native identifiers even under entropy opt-in and an explicit block; protect the same value in ordinary content. Use the current protocol contract, not the older issue proposal.
- **Expectation basis:** [secret-context-classification.md](secret-context-classification.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Incident evidence:** [CLI issue #69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69), [CLI issue #99](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/99).
- **Evidence boundary:** The issue reports the failure; this case reconstructs its local protection boundary.
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.

## incident-codex-call-linkage

- **Coverage kind:** incident-reconstruction.
- **Scope:** policy; plain; runtime: codex; profiles: entropy, all.
- **Scenario:** A Codex function call uses a native call_id which also occurs as an explicitly blocked string inside its JSON-encoded command and result.
- **Expected behavior:** Keep call_id byte-for-byte and preserve the pairing while protecting the content occurrence; no placeholder may be written into native linkage.
- **Expectation basis:** [secret-context-classification.md](secret-context-classification.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Incident evidence:** [CLI issue #69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69), [CLI issue #123](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/123), [Frozen Hub session](https://agent-git.com/@ethanm/coffee-evidence-map/s/agit-758354fc4907620c6ef496cf60810072cac22d64?ref=e173cc9aeb00746ecc558f8d4e957b3e0f45a25c).
- **Evidence boundary:** V1 placeholders are directly visible in the frozen session call_id fields. This fixture uses new synthetic IDs to test preservation rather than replaying the broken historical IDs.
- **Validity checks:** json.

## incident-claude-duplicated-image

- **Coverage kind:** incident-reconstruction.
- **Scope:** policy; plain; runtime: claude-code; profiles: entropy, all.
- **Scenario:** A Claude Read result stores the same small PNG in a structured image block and the native toolUseResult companion, beside diagnostic text.
- **Expected behavior:** Preserve both valid image copies under entropy opt-in and protect the adjacent credential; media evidence must not exempt surrounding text.
- **Expectation basis:** [secret-context-classification.md](secret-context-classification.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Incident evidence:** [CLI issue #71](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/71), [CLI issue #62](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/62), [Frozen Hub session](https://agent-git.com/@einsia/ppt-bench/s/agit-f2ef05924581003865ddaeea71ccbb1c8a53e55b?ref=11b3295de8de76d70752129783626f02a3a7ca87).
- **Evidence boundary:** The frozen Hub session corroborates the image-to-PowerPoint workflow. The duplicate native carrier paths and import refusal are established by issue 71; rendered Hub events do not prove raw carrier equality.
- **Validity checks:** png-base64.

## incident-mcp-image-carrier-equivalence

- **Coverage kind:** incident-reconstruction.
- **Scope:** secret; entropy opt-in; runtime: not runtime-specific; profiles: entropy, all.
- **Scenario:** A tool diagnostic retains both an MCP image result and the equivalent data URL prepared for the model.
- **Expected behavior:** Preserve equivalent valid image bytes in both carriers under entropy opt-in and protect the text credential. This isolates media classification, not the import size budget.
- **Expectation basis:** [privacy-plain-publication.md](privacy-plain-publication.md), [secret-context-classification.md](secret-context-classification.md).
- **Incident evidence:** [CLI issue #62](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/62), [CLI issue #71](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/71), [Frozen Hub session](https://agent-git.com/@ethanm/coffee-evidence-map/s/agit-758354fc4907620c6ef496cf60810072cac22d64?ref=e173cc9aeb00746ecc558f8d4e957b3e0f45a25c).
- **Evidence boundary:** Issue 62 identifies raw MCP data versus an equivalent data URL. The accessible session is already protected history, so original image bytes and the earlier import rejection are not independently recovered here.
- **Validity checks:** png-base64, png-data-url.

## incident-foreign-placeholder-literals

- **Coverage kind:** incident-reconstruction.
- **Scope:** policy; plain; runtime: claude-code; profiles: default, all.
- **Scenario:** A user quotes already-protected history from another dictionary alongside a newly pasted credential.
- **Expected behavior:** Keep unknown canonical placeholders literally and protect the new credential. This checks capture only, not opening-request hydration or cross-device synchronization.
- **Expectation basis:** [secret-context-classification.md](secret-context-classification.md), [privacy-plain-publication.md](privacy-plain-publication.md).
- **Incident evidence:** [CLI issue #60](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/60), [CLI issue #353](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/353), [Frozen Hub session](https://agent-git.com/@einsia/AgentGit/s/agit-f554246de2aaa6b938427df2272f31c06e73267a?ref=618efbbb2a60d67b075d24aa1ac402a08e5b9e4e).
- **Evidence boundary:** The frozen session user prompt directly reports publication hydration is incomplete. Issue 353 distinguishes foreign literals from locally held but unavailable records; this case covers only the foreign-literal input boundary.
- **Validity checks:** corpus ranges, native layout where applicable, and independent annotation review.
