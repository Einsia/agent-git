---
name: agit-telemetry
description: Inspect, preview, enable, or disable account-linked CLI usage statistics.
---

# agit telemetry

Inspect or change account-linked CLI usage statistics. These commands are local,
work without login, and do not themselves produce analytics events.

## Synopsis

```bash
agit telemetry [status]
agit telemetry enable
agit telemetry disable
agit telemetry schema
agit telemetry preview -- <command arguments>
```

## Examples

```bash
agit telemetry status --json
agit telemetry disable
agit telemetry enable
agit telemetry schema --json
agit telemetry preview -- search "example query" --type sessions --local
```

`status` reports the saved preference, effective process setting, environment
override, destination availability, identity state, and unsent queue size.
`schema` prints the command and integration field policies. `preview` prints only
sanitized properties; it does not execute the supplied command or send events.

Setup explains the collection and asks with a default Yes. Noninteractive setup
and `npx -y create-agit` enable it with a visible notice. An existing opt-out
survives reinstallation and `--yes`; only explicit `telemetry enable` reverses it.

`AGIT_TELEMETRY_DISABLED=1` or `DO_NOT_TRACK=1` overrides an enabled preference.
`disable` clears unsent events and stops future collection and uploads, including
from running integrations. It cannot recall requests already sent. To inspect an
enabled foreground invocation locally, `AGIT_TELEMETRY_DEBUG=1` prints sanitized
events to stderr without sending or queuing them; protocol processes stay silent.

The payload contains fixed command names, supported enum/boolean options, quantity
buckets, environment categories, outcomes, and the signed-in account ID when
available. It excludes free-text arguments, shell command text, repository names,
paths, native session IDs, transcript contents, and tokens. Local commands do not
start a telemetry network request. Self-hosted Hubs require an explicit analytics
destination and project key.
