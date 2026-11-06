# Privacy benchmark workflow profiles

The corpus separates protocol probes from reference-backed tool workflows. Passing a
protocol probe establishes envelope routing, Policy selection and call/result linkage;
it does not establish that an agent's default tools or everyday work were exercised.

## Coverage levels

| Case family | Evidence | Scope |
| --- | --- | --- |
| Original boundary cases | Independent Secret/Policy annotations | A focused behavior boundary |
| `native-*` | Synthetic `Read`/`ReadPublic` calls with a common `file_path` argument | Runtime envelopes, source-specific overrides and associations only |
| `scenario-*` | Valid source/configuration/protocol specimens and reviewed user intent | The stated business or credential scenario |
| `workflow-*` | Tool-contract references, native records and an executable project | The declared read/edit/test tool profile and its public/plain preservation |

The workflow traces are constructed, not recordings of live agents. Their tool contracts
were checked on 2026-11-06. They do not measure tool choice, model behavior, approval UX,
or every installed version. Output text is fixed task evidence, not a claim that every
client renders identical status messages. The client wire representation and tool input
types remain explicit, including freeform string arguments.

## Tool profiles

Each row has `workflow-<runtime>-calculator-encrypted-public` and
`workflow-<runtime>-calculator-plain` cases. All perform the same task: read
`src/calculator.py`, fix `double` using the supplied replacement or patch, and run
`python3 tests/check_calculator.py`.

| Runtime/profile | Read | Edit | Test | Contract references |
| --- | --- | --- | --- | --- |
| Claude Code | `Read(file_path)` | `Edit(file_path, old_string, new_string)` | `Bash(command, description)` | [Tools reference](https://code.claude.com/docs/en/tools-reference), [captured input schemas](../tests/fixtures/cc-2.1.311-trailing-records.jsonl) |
| Claude Desktop + Desktop Commander MCP | `read_file(path)` | `edit_block(file_path, old_string, new_string)` | `start_process(command, timeout_ms)` | [MCP server schemas](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts) |
| Cursor transcript profile | `Read(path)` | `ApplyPatch` with a string | `Shell(command, working_directory)` | [checked-in transcript conventions](../src/adapter/cursor.rs), [agent tools](https://docs.cursor.com/en/agent/tools) |
| Codex transcript profile | `exec_command(cmd, workdir)` | `apply_patch` as a freeform `custom_tool_call` | `exec_command(cmd, workdir)` | [native call/result variants](../src/adapter/codex.rs) |
| OpenCode V2 | `read(path)` | `edit(path, oldString, newString)` | `shell(command, workdir)` | [V2 tools](https://dev.opencode.ai/v2/docs/tools/) |
| Hermes file/terminal toolsets | `read_file(path)` | `patch(path, old_string, new_string)` | `terminal(command)` | [file editing and test pipeline examples](https://hermes-agent.nousresearch.com/docs/user-guide/features/code-execution) |
| OpenClaw with Pi batched edit | `read(path)` | `edit(path, edits[{oldText, newText}])` | `exec(command, workdir)` | [agent tools](https://docs.openclaw.ai/concepts/agent), [Pi edit schema](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/tools/edit.ts), [exec parameters](https://docs.openclaw.ai/tools/exec) |
| WorkBuddy + Desktop Commander MCP | `read_file(path)` | `edit_block(file_path, old_string, new_string)` | `start_process(command, timeout_ms)` | [WorkBuddy MCP support](https://www.codebuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/MCP-Guide), [server schemas](https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts) |

The Desktop Commander rows require that server to be configured and available to the
client, using its advertised tool names. They are not claims about built-in Claude
Desktop or WorkBuddy tools. Client-specific name prefixes and other MCP configurations
require their own profile. The OpenCode row is V2, and the OpenClaw edit row is the
documented batched Pi schema; other versions must not be silently relabeled as these
profiles. Cursor and Codex use the checked-in native transcript conventions rather
than assuming their tools have Claude Code argument names.

## Independent replay

`benchmarks/privacy/workflows.py` contains the fixed project, tool arguments, results
and native bindings. The input file returns `value + 1`; the edit changes that to
`value * 2`. The existing checks cover positive and negative inputs.

`python3 benchmarks/privacy/validate_fixtures.py --external` verifies the tool profile,
step order, read contents, replacement match and patch framing. It then writes only
the fixed calculator project to a temporary directory and runs its Python check script
before and after the edit. The initial program must fail an assertion and the edited
program must print the recorded success result. The recorded shell commands are checked
as data, not executed; no agent or MCP server is launched.

The privacy expectation is independent of that replay: preserve ordinary code, patch
text, results and associations; alias local paths in a public view; preserve native
records in plain capture. A public projector that cannot represent a legal custom
tool call should fail the benchmark. Recasting that call as a JSON function merely
to obtain a passing score would invalidate the fixture.

See the [case catalog](privacy-benchmark-cases.md) for every case and the
[benchmark guide](privacy-benchmark.md) for execution and recorded findings.
