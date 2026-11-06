# Incident-derived privacy benchmark cases

## Evidence reviewed

On 2026-11-06, keyword searches of the CLI project's open and closed issues returned
118 distinct matches. The review screened their metadata and read 19 relevant report
bodies. This is a targeted review, not a claim that every issue or Hub session was
audited. Global and repository-scoped Hub content searches returned `incomplete: true`
with no hits; those responses do not establish absence of relevant history.

Read-only Hub catalog requests located the following accessible snapshots. The saved
references were frozen before reading their rendered turns. No repository was adopted,
no session was resumed, and no original dictionary or credential was imported.

| Frozen session | What was actually observed | Evidence limit |
| --- | --- | --- |
| [Night Owl](https://agent-git.com/@leopierce/night-owl-index/s/agit-f7d113cee8eb65f2e207e8365799680fce623122?ref=95f9834d4d00a0818957397562a34cddf68900bb) | `SimpleHTTPRequestHandler` in a server import and subclass | Corroborates the source-code workflow; issue 66 establishes the later file-commit corruption |
| [Weather Poetry](https://agent-git.com/@jonahreid/weather-poetry/s/agit-a2ee913d7202709cd1acb9803e580992d34b4a7d?ref=942d3a2713f4f5efa511ec3f98472da2e96f4aae) | Relative PDF output references and a README delivery request | The readable snapshot does not prove that a broken README was published |
| [Deep Time](https://agent-git.com/@zoesullivan/deep-time-redesign/s/agit-4cefd98071a1fb4069d5d8c2fc63ab5b122dca42?ref=45187fe3db1a893d6adf9aab66500fc88fa431fd) | Foldout PDF references, generated artwork and source handoff | Another workflow corroboration for issue 67 |
| [Presentation reconstruction](https://agent-git.com/@einsia/ppt-bench/s/agit-f2ef05924581003865ddaeea71ccbb1c8a53e55b?ref=11b3295de8de76d70752129783626f02a3a7ca87) | `MSO_ANCHOR.MIDDLE` and `MSO_SHAPE` references in generated Python, with an image-based task | Issue 71 supplies the raw duplicate-carrier paths; rendered turns alone cannot prove raw image equality |
| [Coffee evidence map](https://agent-git.com/@ethanm/coffee-evidence-map/s/agit-758354fc4907620c6ef496cf60810072cac22d64?ref=e173cc9aeb00746ecc558f8d4e957b3e0f45a25c) | V1 placeholders visibly occupy rendered `call_id` fields and public-reference URL text | Direct reader-visible anomaly; original hidden values and the exact stage that replaced them were not recovered |
| [Publication repair](https://agent-git.com/@einsia/AgentGit/s/agit-f554246de2aaa6b938427df2272f31c06e73267a?ref=618efbbb2a60d67b075d24aa1ac402a08e5b9e4e) | The user's opening report contains the publication-hydration failure | Access-controlled session; corroborates the error, not a replay of the entire publication failure |

These are rendered Hub snapshots, not a byte audit of every native LOG or historical
file tree. Original transcript text, usernames, local paths, IDs, media and credentials
are not copied into case inputs. Only the failure shape is reconstructed. Public API
symbols needed to establish a non-secret role, such as `SimpleHTTPRequestHandler`,
remain as their real public names.

## Added cases

The `evidence` metadata on each case links its reports and frozen session where one
was read. The [case catalog](privacy-benchmark-cases.md) describes the scenario and
expected output in detail.

| Case | Reports / observations | Contract added |
| --- | --- | --- |
| `incident-stdlib-import` | [#66](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/66), Night Owl | Preserve standard-library imports and inheritance under entropy opt-in; still protect an adjacent credential |
| `incident-relative-artifact-links` | [#67](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/67), Weather Poetry and Deep Time | Preserve Markdown artifact destinations and labels without exempting adjacent credentials |
| `incident-ppt-enum-references` | [#69](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/69), presentation snapshot | Preserve dotted enum/module references under entropy opt-in while protecting a literal credential |
| `incident-project-key-without-marker` | [#114](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/114), [#136](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/136) | Protect a long provider-shaped project key in ordinary prose without requiring an embedded vendor marker |
| `incident-beta-flags-and-usage` | [#99](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/99), #69 | Keep feature flags and token-count metadata, protecting the separate Authorization value |
| `incident-public-reference-urls` | Coffee snapshot, #69 | Preserve public resource references and ordinary query fields; protect a separate credential-bearing query |
| `incident-claude-native-identities` | #69, #99 | Preserve verified UUID and tool-link fields while protecting the same explicitly blocked value in ordinary content |
| `incident-codex-call-linkage` | Coffee snapshot, [#123](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/123), #69 | Preserve call IDs and their association while protecting content occurrences in encoded arguments and output |
| `incident-claude-duplicated-image` | [#71](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/71), presentation workflow | Preserve a valid PNG in both the image block and `toolUseResult.file.base64`, without exempting adjacent text credentials |
| `incident-mcp-image-carrier-equivalence` | [#62](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/62), coffee workflow | Give equivalent valid raw MCP image data and data URL bytes the same retention decision |
| `incident-foreign-placeholder-literals` | [#60](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/60), [#353](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/353), publication-repair snapshot | Preserve quoted foreign canonical placeholders as literals while protecting newly supplied credentials |

The project-key reports explicitly used generated fake values. The fixture preserves
that troubleshooting scenario; it does not assert that the replacement is a working
provider credential. The image reconstruction uses a newly generated valid PNG with
matching MIME and identical bytes in each carrier. It isolates classification from
the historical import-size limit and does not reproduce the report's mismatched JPEG
declaration. Protocol-ID expectations follow the current documented contract; the
older proposal in issue 69 to scan provider patterns inside those IDs is not copied
as an oracle.

## Deliberately separate integration work

| Evidence | Why a scanner/projection case would be insufficient |
| --- | --- |
| [#310](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/310), typed-constant miss | The report withholds the actual form. No case is invented from the title or detector implementation; obtain the original non-sensitive reproducer first. |
| [#180](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/180), Overflow question ID refused | Requires the MCP forwarding field-selection and diagnostic contract. Exempting a generic JSON field named `id` in the detector would test the wrong rule. |
| [#245](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/245), [#279](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/279), cross-device rules/restoration | Requires separate devices/accounts, rule generations, package synchronization and unavailable keys. A fresh local dictionary cannot establish propagation. |
| [#343](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/343), publication under newer rules | Requires captured history, changed rules and inspection of the sent Git objects. Running the current scanner once is not that test. |
| #353, first-publication hydration | The foreign-literal case covers capture only. First-prompt selection, foreign versus locally locked records, and publication refusal need publication integration. |
| [#363](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/363), [#364](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/364), [#370](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/370) | Store contention, retry/repreview and status explanations require controlled concurrency and transport outcomes. |
| [#304](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/304), home names in command text | The report asks for a policy decision. Automatic anonymization expectations must follow the selected policy rather than assume every plain transcript hides all local paths. |
| [#314](https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/314), injected tags in Codex output | The report is a static consistency follow-up and explicitly lacks a real rollout reproducer at that point. It needs the publication-reduction provenance contract, not an arbitrary text-tag deletion rule. |
| #62, #71, large import/overlap refusal | The valid-image cases establish carrier behavior. Size budgets, all-or-nothing import and dictionary side effects need a separate importer/stress check. |

Issues 66 and 67 also request end-to-end file-commit checks against stored bytes and a
fresh reader. Their new cases cover only the text-protection boundary. Existing
memory allowlist checks already cover the local decision relevant to issues 143/349;
the remaining reports concern discoverability and command UX, not missing corpus rows.

## Running and interpreting

Dataset `1.5.0` adds 11 cases. `all` contains 98 cases, `default` contains 87, and
`entropy` contains 11. The entropy profile now includes the opted-in native/media
Policy cases as well as Secret text cases and its default-mode control.

Use the ordinary benchmark runner and the independent fixture validator. An older
report being closed does not require a new case to fail today. Likewise, a new failure
does not justify changing valid input or weakening an independent expected boundary.
Preserve the raw worker evidence, distinguish completed protection from structural
verification, and report any integration boundary that the case does not exercise.

The local CLI `0.2.42` check completed all new cases. Import/reference, provider-prefix,
metadata, Claude identity, public URL and foreign-placeholder expectations passed.
The duplicated Claude image and raw MCP image each had an unwanted replacement while
the equivalent image copy survived. Codex call IDs remained intact, but the blocked
content occurrence caused its entire encoded argument to become opaque, failing the
independent argument and structural checks. The worker completed that request; the
report retains its output rather than mislabeling a JSON decoding exception as the
only available evidence. These observations are local component results, not fresh
reproductions of every historical import, push or cross-device failure.
