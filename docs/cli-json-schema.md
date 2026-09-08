# agit CLI JSON output

`agit --json <command>` (and the compatible `agit <command> --json` spelling)
emits exactly one JSON document on stdout. The normative machine-readable
default contract is [`cli-json-schema-v2.json`](cli-json-schema-v2.json), checked into the
source tree as the single source of truth. A future packaging step can install
that file next to the binary for local validation, but the output itself does
not depend on an installation path. It deliberately carries the stable schema
name `cli-output` and `schema_version: 2` instead of pointing at an assumed
online URL.

## Envelope

Every supported command uses the same top-level fields:

- `schema`: the stable schema name (`cli-output`). It is intentionally not a
  network URL; installations may live in different prefixes or be offline.
- `schema_version`: currently `2`; consumers should reject or explicitly
  negotiate versions they do not understand.
- `command`: the canonical top-level command name.
- `ok`: `true` exactly when `exit_code` is zero.
- `exit_code`: the normal CLI exit code.
- `result`: command output, represented as `json`, `json_lines`, `text`, or
  `empty`.
- `diagnostics`: captured stderr diagnostics as `{level, message}` objects.
  Stdout is not duplicated: it belongs exactly once in `result`.
- `fix`: alternative typed recovery commands. The CLI reports them without execution;
  see [recovery actions](cli-json-fixes.md).

Use `--json --json-version 1` to retain the legacy envelope and validate it with
[`cli-json-schema.json`](cli-json-schema.json). That version has no `fix` field.
The original version 1 schema is preserved unchanged.

`result.format=json` preserves an existing structured command value under
`result.value`; it is not JSON encoded as a string. JSONL output uses
`result.format=json_lines` and decoded `result.values`. Human-oriented commands
temporarily use `result.format=text` with newline-free, non-empty `result.lines`;
blank presentation lines are omitted. Future iterations can add command-specific
`kind` values and structured fields while
keeping this envelope stable.

The `whoami` command is the first command-level structured result. Its
`result.value` contains `hub`, `account`, optional `email`, non-secret access
and refresh token states with expiry timestamps, and `check` fields including
whether an online check was requested and whether the server was reachable.
Token values themselves are never included.

The hidden `hooks` and `mcp` commands are excluded: they own stdin/stdout as
line-oriented protocols, so wrapping their stream would make the protocol
invalid. Their existing protocol formats remain unchanged.
