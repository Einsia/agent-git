# CLI recovery actions

`agit --json <command>` emits the version 2 envelope described by
[`cli-json-schema-v2.json`](cli-json-schema-v2.json). Use
`--json --json-version 1` for the preserved version 1 envelope and
[`cli-json-schema.json`](cli-json-schema.json). An explicit `--json-version 2`
selects the default contract. A consumer must select a version it understands;
adding a property to the closed version 1 schema would not be compatible.

Version 2 preserves command output in `result` and human diagnostics in
`diagnostics`. Its `fix` array contains alternative, complete next-step commands.
It is empty on success and when no safe, complete action is known. The CLI only
reports these actions. It never executes them, schedules retries, or treats an
action as authorization.

Each action has `kind: "agit_command"`, a native `argv` array whose first value is
`agit`, an absolute `cwd`, non-secret routing overrides in `env`, and a
`requires_interaction` flag. A consumer uses its known AgentGit executable,
passes each remaining array entry as one native argument, and retains the
original invocation's environment with the listed routing overrides. It must
not pass the action to a shell, split values on whitespace, or interpret shell
metacharacters. The array represents alternatives, not a sequence to run.

Known literal values such as `<work>`, leading `@`, quotes, newlines, empty
arguments, and trailing backslashes remain data. Missing input is not represented
by a placeholder action. A NUL or a native argument/path that cannot be represented
losslessly as a JSON string prevents that action from being emitted. Token values,
credential-bearing URLs, arbitrary environment values, and runtime authority
markers are never copied into actions.

Recovery actions require a valid Hub address and the raw lowercase `http://` or
`https://` scheme spelling declared by the action schema. Unsupported spellings remain
diagnostics without an action; reporting never rewrites routing or credential
selection to manufacture a runnable command.

A recovery for an already selected request Hub retains that same Hub in its
arguments and routing overrides. A concurrent global configuration change cannot
retarget the recovery or make those values disagree.

Recovery actions currently cover signing in to the selected hub, cloning an
explicit repository that is absent locally, and preparing an explicitly named
resume target when a launched runtime is incompatible with JSON output. Parsed
runtime, cwd, force, and explicitly supplied confirmation/presentation flags are
preserved for the preparation retry. Option values use the equals form and the
positional target follows `--`, so values starting with a hyphen stay values. Missing
or ambiguous target information remains a diagnostic without an invented fix.

A missing-repository recovery clones with `--no-bind`: downloading the selected
session's repository preserves the workspace's existing creation route, including
the absence of a binding.

Preparing a resume can still ask about changed workspace state or a lossy runtime
conversion. Its action conservatively requires interaction unless the original
invocation already supplied `--yes` or a Unicode `AGIT_YES` value. Reporting never
adds confirmation on the caller's behalf. The consumer retains that original
environment when executing the action.

Human `hint` messages remain diagnostic text. Only typed registration contributes
action data; text printed by a subprocess cannot register an action. Workers that
contribute recovery actions carry the current reporter explicitly and use its
`run` method. Reporter handles close with their invocation, including on failure
or unwinding, and cannot publish into a later invocation.

Hub HTTP errors can include `fix: [{"kind":"authenticate"}]`. The client accepts
this recipe only with HTTP 401 and `kind: "unauthorized"`, retains it as passive
error metadata, and reports a login action only when that error determines the
command's final failure. A missing or rejected recipe also prevents the generic
HTTP error renderer from generating a Hub-login command: other credential failures
can use the same status and category. The server's diagnostic remains intact.
Failed refresh exchanges, discarded
probes, warnings and recovered requests do not register actions. The login action
uses the failed request's Hub even when current global routing has changed.

Only locally implemented recipe shapes are accepted. Unknown kinds, malformed
entries and absent fields preserve the original error without inventing recovery.
An HTTP status or human hint alone does not produce an action. Server-provided
commands, arguments, environments and destinations are never used. Explicit
JSON version 1 continues to omit the action array.

The native Windows MSVC build captures command output from both Rust and the C
runtime, including inherited subprocess output. It supports the same version
selection and typed recovery data. Capture setup must succeed before the command
runs. Output exceeding 64 MiB on either stream, a capture failure, or a child still
holding an output stream after the command returns produces an explicit incomplete
capture error. If the final JSON destination is closed, a successful command exits
with a nonzero code; the command's side effects may already have completed.

HTTP recipes are translated into this CLI envelope; JSON-RPC error protocols
remain separate.
