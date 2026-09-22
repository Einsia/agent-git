# CLI usage statistics

AgentGit collects account-linked usage statistics for the official Hub to improve its CLI and Hub.
Private and public repositories use the same field restrictions. Repository upload
and usage statistics are separate operations: allowing one does not make repository
contents eligible for the other.

## Hub policy

Usage statistics are required for `https://agent-git.com` and
`https://www.agent-git.com`, including an explicit HTTPS port of 443. The selected
Hub determines this policy; `agit login --hub` uses its explicit Hub for the whole
invocation. Other hosts, staging, HTTP and nonstandard ports do not inherit it.

Official Hub commands initialize statistics in interactive, redirected, JSON, CI,
hook, MCP and RC invocations. A saved disabled preference is migrated to enabled;
an existing enabled installation keeps its identity and install deduplication.
The prior optional preference is retained for later use of a nonproduction Hub.
`AGIT_TELEMETRY_DISABLED`, `DO_NOT_TRACK`, `AGIT_TELEMETRY_DEFER` and
`AGIT_TELEMETRY_DEBUG` do not disable collection or sending for the official Hub.
Setup displays an informational notice and does not ask for a telemetry choice.
The `agit telemetry` subcommand is removed.

Help, version, invalid invocations, completion generation and
`setup --skill --installed-only` remain lightweight and do not initialize statistics.
Local commands buffer events without starting a sender. Mandatory collection does
not make local operations require network access or make delivery guaranteed.

Other Hubs have no implicit analytics destination. Their setup preference and
`AGIT_TELEMETRY_DISABLED=1` / `DO_NOT_TRACK=1` overrides remain effective; a saved
refusal is preserved. `AGIT_TELEMETRY_DEFER` continues to defer their hidden
installer setup. With an enabled preference and an explicitly configured
nonproduction destination, `AGIT_TELEMETRY_DEBUG=1` prints sanitized events to
stderr instead of queuing or sending; protocol processes suppress that output.

## Collected fields

The checked-in [`registry.json`](../src/telemetry/registry.json) is the complete command-field policy.
It covers canonical top-level commands, nested subcommands, aliases, MCP tools and
inputs, and RC protocol methods. Tests reject unclassified additions. Aliases are
reported under their canonical command name.

| Field category | Representation |
| --- | --- |
| Commands | Fixed command and subcommand names |
| Boolean options | Boolean values, including their parser defaults |
| Enum options | Reviewed values, or `other`; repeated enums are deduplicated |
| Numeric quantities | Coarse buckets, including limits, page numbers, view caps and list lengths |
| Free-text inputs | Whether supplied, or a quantity bucket for repeated inputs |
| Configuration | Fixed keys and approved enum/boolean values; custom values remain presence-only |
| Identity | Authoritative Hub account ID when available; random telemetry identifiers |
| Environment | OS, architecture, normalized language, CPU-count bucket, CI category, independent stdin/stdout/stderr TTY flags |
| Calling context | Direct, agent hint, hook, MCP, RC or installer; foreground/background and protocol flags |
| Agent environment | Known runtime-marker presence, `AGIT_SESSION` presence and syntax validity, merge/RC flags |
| Resolved facts | Runtime, bound workspace, managed-context result, prompt/TUI entry, and settled-turn count when observed by the product |
| Results | Fixed exit category/code, failure stage, duration bucket and operation outcome |

`AGIT_SESSION` is the managed-session environment variable. `AGIT_SESSION_ID` is
only a presence diagnostic. Neither value is transmitted, and a syntactically
valid environment value does not prove a live managed session. Unknown facts stay
unknown; telemetry does not scan transcripts or repositories to fill missing fields.

Excluded values include command lines, shell command text, free-text arguments,
repository/branch names and IDs, filenames and paths, native session IDs, search
queries, prompts, transcripts, file contents, credentials, email, username, raw
errors, stack traces, URLs, terminal input and terminal output. Numeric resource
IDs such as pull-request IDs are presence-only, not numeric measurements. Secret
scans report only outcomes and count buckets, never findings or matched values.
No hardware fingerprint, process-command-line inspection, screen recording or
automatic event capture is used.

## Identity and activity

The CLI retains `account_id` from login responses and normal `whoami --check`
responses. Existing credentials without this field remain usable and report
`signed_in_id_missing` until an ordinary authentication request supplies it.
Analytics reads a tokenless sidecar validated against the selected credential
file's metadata; it never refreshes tokens or requests identity solely for analytics.

Production `distinct_id` matches Hub's authoritative account ID. Anonymous users
receive a random namespaced ID; nonproduction accounts use an environment namespace.
Account switching rotates the telemetry device and activity identifiers, without
rewriting queued events or aliasing accounts through a shared machine. Running
invocations stop emitting when their captured account or preference changes.

A foreground activity session rotates after inactivity, at the UTC date boundary,
or after an account change. Background hook, MCP-parent and daemon activity does
not extend foreground activity. These IDs are independent of agent conversations.
RC request counters describe activity on the local account's machine; they carry
`actor_kind=remote_operator`, not an inferred collaborator account ID.
Requests update fixed in-memory counters; a background worker periodically merges
them into the disk queue. Terminal input and resize handling perform no telemetry
disk I/O. The worker rechecks the captured account and consent generation before
writing, so disabling statistics discards delayed counts. Counts that have not
been persisted when the daemon exits may be lost.

## Events and interpretation

| Event | Meaning |
| --- | --- |
| `cli_command_started` | An external invocation entered the CLI |
| `cli_command_finished` | The invocation exited, including parse/startup/JSON admission failures |
| `cli_operation_finished` | A Hub/Git request, runtime launch, settlement, secret scan, artifact transfer, MCP tool or TUI screen returned |
| `cli_integration_summary` | Aggregated hook completions or received RC requests |
| `cli_onboarding_completed` | Setup established an enabled preference |
| `cli_session_started` | Foreground telemetry activity started a new session |
| `cli_install_stage` | Visible create-agit attempt start, durable copy, binary verification, integration setup and completion |
| `cli_install_succeeded` | An installer copied the binary and verified it runs; once per installation generation |
| `cli_install_attributed` | A tagged installer associates an installation with an anonymous website acquisition |
| `cli_acquisition_linked` | The first successful CLI login saved an authoritative account ID |

Use `cli_command_finished` and `invocation_id` for command counts. Operation events
are detail within an invocation, not additional commands. MCP child commands carry
a telemetry-only parent ID and a fixed tool name. Hook summaries use
`integration_count`; RC summaries use fixed RPC names and `outcome=received`, which
is not proof the operation succeeded. Heartbeats and streamed output are ignored.
TUI picker operations include a selection count of zero when dismissed.

An operation returning `ok` means that operation returned successfully. A successful
secret scan can still contain findings; inspect its count bucket. Browser login
handoff records `authorization_pending`, not authenticated login. A started event
without a finished event can be an interrupted process or lost delivery and must
not automatically be counted as a crash. Analytics is best effort and is not an
audit log or a complete denominator for setup acceptance.

## Buffering and sending

Preferences and the unsent queue live under `AGIT_HOME/telemetry`, outside code and
AgentGit repositories. Files are private to the local user. The queue is bounded
by both event count and encoded size, individual events have a size limit, and
unsent events expire after 24 hours. The sender uses bounded batches, a short HTTP
timeout, backoff and stable event UUIDs/timestamps on retries. Hooks and received
RC requests aggregate within a bounded interval.

Offline operations enqueue only. The CLI starts a detached sender after an online
operation or enabled setup; long-running online integrations can periodically drain
queued events. Local-only installations may never upload their queue. Failed
telemetry never changes the product command's output or exit code. There is no
network wait on the ordinary command's exit path.

Installation generations prevent stale collectors from appending events after an
identity reset. Deleting already received data is separate from local state.

## Destinations and PostHog

The official production CLI uses the Hub frontend's public PostHog ingestion token
and US ingestion endpoint. No Hub authorization header is attached. Self-hosted,
staging and development Hubs have no implicit official analytics destination.
Operators can explicitly set both `AGIT_TELEMETRY_HOST` and
`AGIT_TELEMETRY_KEY` to a dedicated project. HTTPS is required except for loopback
test receivers. Credentials, query strings and redirects in telemetry destinations
are refused. On the official Hub, invalid or incomplete destination overrides fall
back to the built-in destination. Queues are partitioned by the selected Hub and analytics destination;
changing either cannot route old records into another project.

The payload excludes IP addresses and requests that PostHog disable GeoIP enrichment.
As with any direct network connection, the ingestion service can still observe the
connection's source IP. Cloud retention and access controls follow the actual
PostHog project configuration; the local queue's expiry does not establish cloud
retention. The API envelope follows PostHog's
[capture and batch contract](https://posthog.com/docs/api/capture).

The module is gated by the Cargo `cli` feature. Backend consumers using
`default-features = false` do not link or execute this telemetry system.

## Hub download attribution

Separately from usage statistics, the Hub counts repository downloads and visits
through shared links. Git and Git LFS requests that agit sends to the configured
Hub carry two headers in addition to authorization and repository identity:

| Header | Value |
| --- | --- |
| `X-AgentGit-Command` | The top-level command name, such as `clone`, `pull`, `fetch`, `run` or `new`; omitted when no command is dispatched |
| `X-AgentGit-Operation` | A random UUID generated once per agit process and never stored |

The Hub uses them to count one download per invocation and repository, so
credential retries and negotiation rounds are not counted again. They contain no
arguments, repository, branch or session names, paths or machine identifiers.
They are scoped to the validated Hub repository URL like the authorization
header: other Git remotes, Hub REST calls and PostHog never receive them. The
usage statistics opt-outs above do not remove them, because the Hub serving the
request already knows the account and repository it concerns.

Session page links printed by `agit show` and `agit file link` append
`sharer=<username>` when an account is signed in to that Hub, so the Hub can
credit visits that arrive through a pasted link. Signed out, the link is
unchanged.


## Acquisition funnel contract

Website `page_view` and anonymous `product_intent` events carry a random
`acquisition_id`. Copying a create-agit install command adds the opaque
`--acquisition-id <UUID>` option. The installer passes it to the verified binary
as `AGIT_ACQUISITION_ID`; only random UUIDs are accepted, never command text.
The install receipt carries `installation_id` and the optional acquisition key.
A tagged reinstall can emit `cli_install_attributed` to associate an existing
receipt without counting another installation. These events are independently
sent even when setup is skipped or fails and no login ever follows. Official Hub
installs record and queue the verified receipt even when npm lifecycle output is
hidden; `--defer-notice` only suppresses the notice there. Other Hubs retain the
optional preference and deferred receipt behavior: visible setup can admit a
pending receipt with its original timestamp. Local-only commands do not start a
sender, and pending receipts expire after a day.

The first `agit login` handoff URL also carries `installation_id`. The browser
stores it through registration, so a direct npm installation can be associated
with a website visit when the browser authorization page is opened. The CLI emits
`cli_acquisition_linked` only after credentials containing the authoritative
account ID are saved, and only for the first completed acquisition. The first
account is persisted under the installation gate at credential save, independently
of event buffering. Queue failures retry that same account, event ID and timestamp;
a later login cannot claim the installation. Existing
account login is not registration; use website `signup_success` for that stage.
The ordinary started/finished command events continue to describe pending,
failed, and completed login attempts.

Build the funnel by joining website acquisition keys to installation receipts
and the first account link, then counting distinct resolved people at each stage.
Exclude installer events with `ci=true` from human acquisition cohorts.
Deduplicate multiple intent events as one person. Never sum clicks or equate a
copied command, a download, setup completion, or a pending authorization with an
installation or registration. Multiple devices require the authoritative account
link to deduplicate as people; unregistered devices remain anonymous installations,
not a provable count of natural persons. These keys do not alias all anonymous CLI
activity or merge subsequent accounts sharing a machine.

Untagged installs that never open browser authorization cannot be connected to an
earlier website visit. Where the installation receipt was eligible for delivery, they still appear in
the installation-without-observed-registration cohort. Manual archive copies, builds outside the installers, `--no-verify`, npm
`--ignore-scripts`, nonproduction opt-outs, offline delivery expiry, and older CLI releases have
no verified install receipt. Exclude those from claims of complete coverage. Data
is prospective; deploying the website and releasing the CLI are both required.

## Local campaign attribution

`npx -y create-agit --campaign-url <url>` passes campaign context to the verified
installation receipt. Native/global installers can use `AGIT_CAMPAIGN_URL`.
The CLI stores `campaign_first` and `campaign_latest` in its private telemetry
preferences, including capture time and decoded query values. Repeated values
are preserved. These fields are local context for later account attribution;
they are not added to outbound CLI events or used to merge PostHog people.

Accepted fields are lowercase `utm_*`, `campaign`, `gclid`, `dclid`, `gbraid`,
`wbraid`, `fbclid`, `msclkid`, `ttclid`, `twclid`, and `li_fat_id`. Paths,
fragments, credentials and unrelated query fields are not stored. Input is
bounded to 8192 URL bytes, 32 keys, 8 distinct values per key and 1024 bytes
per value; invalid or excess fields are ignored without blocking installation.
The first context is retained across reinstalls, while the latest is updated.
The selected Hub policy and destination boundaries apply. Nonproduction hidden
installations can defer the receipt until setup establishes a preference.

### Visible installer stage coverage

`cli_install_stage` uses the selected Hub policy, route binding and bounded queue.
The visible installer reports that statistics are required for the official Hub
before copying. Other Hubs retain their optional setup preference. A stage does not set the verified-install flag or create a
receipt. Repeated installs receive distinct random `attempt_id` values while
retaining the installation identity.

Stages are `started`, `binary_copy`, `verification`, `setup`, and `finished`.
Outcomes are `started`, `ok`, `error`, `skipped`, and `partial`. The error
classification is limited to `none`, `filesystem`, `binary`, or `integration`;
raw errors, paths, command text, campaign URLs and runtime session values are
never stage properties. `elapsed_ms` measures the stage, or the complete visible
attempt for `finished`, and is capped at a day. The runtime label uses the same
environment-name allowlist as ordinary command telemetry.

`coverage=visible_installer_after_package_fetch` explicitly excludes npm package
download time and failures before the bundled binary can run. An attempt with
no terminal event is incomplete or unobserved, not an asserted installation
failure. Setup failure produces a partial completion because the verified
binary remains installed. Missing telemetry, including nonproduction opt-outs and offline
lifecycles, must never be counted as installation failure.
