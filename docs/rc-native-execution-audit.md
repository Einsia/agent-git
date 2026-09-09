# Native RC execution audit

Evidence date: 2026-09-08. Host: macOS. Installed runtimes: Codex CLI 0.151.0 and
Claude Code 2.1.263. This audit exercised the production CLI supervisor and Codex driver in
an empty temporary directory, with Plan mode, all tool approvals denied, an isolated
`AGIT_HOME`, and no AgentGit repository lineage. It did not publish anything or drive an
existing daemon/session.

## Native acceptance, steering and transcript provenance

An isolated supervisor received a fixed-token prompt attributed to synthetic `audit-alice`.
After the native runtime returned exact acceptance, a second message attributed to synthetic
`audit-bob` steered the active turn. These identities were supplied at the supervisor boundary;
Hub authentication is covered separately by the Hub tests.

Observed with `gpt-5.4-mini`, selected from this installed runtime's actual `model/list` result:

- `turn/start` returned exact acceptance with a native turn ID.
- `turn/steer` returned immediate delivery, and `turn.steered` preserved Bob's distinct sender
  and client message ID. `turn.started` preserved Alice's corresponding metadata.
- The native transcript contained both prompts and assistant replies `RC_AUDIT_START` and
  `RC_AUDIT_STEER`. The turn completed with `outcome: "ok"`.
- All 11 emitted transcript-item hashes matched the canonical SHA-256 prefixes of their exact
  native transcript lines. Native `turn_context` metadata named `gpt-5.4-mini`.
- Native response items contained messages and reasoning, with no function/tool calls. The
  temporary working directory remained empty. The supervisor and native child stopped.

Codex supplied no dollar-cost field; absence was not interpreted as zero cost. These are local
execution observations, not production regional-latency measurements or a live browser-to-Hub
acceptance test.

## Defects exposed and repaired

The user's configured default, `gpt-6-astra`, was accepted by the native turn protocol but the
provider then refused sampling with HTTP 400 and an instruction to upgrade Codex. The completion
originally reached RC without a reason. The driver now carries native `Turn.error.message` into
`turn.completed.error` as a string, after persona and registered-secret redaction. A repeat
native run verified that the actionable compatibility error reaches the RC event.

`LaunchSpec.model` was honored by Claude but omitted by Codex. Codex now forwards an explicit
model to both `thread/start` and `thread/resume`, while absence preserves the runtime's default.
The successful run's native model metadata confirms the override took effect; the user's
configuration was not changed.

The `rc_smoke` example did not acknowledge Codex's Ready barrier and could also print `OK` after
an errored turn. It now waits for native readiness, acknowledges the fresh-start barrier,
requires a successful completion, and awaits shutdown. A real fixed-token run completed with
8 transcript items. `AGIT_SMOKE_DENY_TOOLS` selects Plan mode and denies approval requests.

## Reproduce the bounded completion smoke

First inspect the installed runtime's authentication and model catalog. The model below was
advertised during this audit; account availability can change.

```sh
env -u AGIT_SESSION -u AGIT_EXPECTED_AGENT_ID -u AGIT_MERGE_TX \
  AGIT_HOME="$(mktemp -d)" AGIT_SECRETS_KEYSTORE=file \
  AGIT_SMOKE_DENY_TOOLS=1 AGIT_SMOKE_MODEL=gpt-5.4-mini \
  cargo run --locked --example rc_smoke -- codex \
  'Do not use any tools or read any files. Reply with exactly: RC_EXAMPLE_OK'
```

The installed protocol schema was generated with `codex app-server generate-ts`. It declares
`Turn.error` as an optional `TurnError` object with a string `message`, and optional string
`model` overrides on both thread-opening methods. See the
[official app-server reference](https://learn.chatgpt.com/docs/app-server).

Claude reported `loggedIn: false` and `authMethod: "none"`. No Claude login or model call was
attempted, so native Claude completion remains unverified on this host. No CLI installation or
upgrade was performed.
