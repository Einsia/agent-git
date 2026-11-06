"""Construct reference-backed read/edit/test traces without consulting privacy output."""
import json
import re

BEFORE = 'def double(value):\n    return value + 1\n'
AFTER = 'def double(value):\n    return value * 2\n'
TEST = '''from pathlib import Path
import runpy

double = runpy.run_path(str(Path(__file__).resolve().parents[1] / "src/calculator.py"))["double"]
assert double(3) == 6
assert double(-2) == -4
print("calculator checks passed")
'''
PATCH = '*** Begin Patch\n*** Update File: src/calculator.py\n@@\n def double(value):\n-    return value + 1\n+    return value * 2\n*** End Patch\n'
RUN = 'python3 tests/check_calculator.py'
TARGET = '{workspace}/src/calculator.py'
DC_SOURCE = 'https://github.com/wonderwhy-er/DesktopCommanderMCP/blob/main/src/tools/schemas.ts'
PROFILES = {
    'claude-code': ('Claude Code Read / Edit / Bash',
                    'tests/fixtures/cc-2.1.311-trailing-records.jsonl; https://code.claude.com/docs/en/tools-reference'),
    'claude-desktop': ('Claude Desktop with Desktop Commander MCP', DC_SOURCE),
    'cursor': ('Cursor Read / ApplyPatch / Shell, checked-in transcript conventions',
               'src/adapter/cursor.rs; https://docs.cursor.com/en/agent/tools'),
    'codex': ('Codex exec_command / freeform apply_patch', 'src/adapter/codex.rs'),
    'opencode': ('OpenCode V2 read / edit / shell', 'https://dev.opencode.ai/v2/docs/tools/'),
    'hermes': ('Hermes file and terminal toolsets',
               'https://hermes-agent.nousresearch.com/docs/user-guide/features/code-execution'),
    'openclaw': ('OpenClaw read / Pi batched edit / exec',
                 'https://docs.openclaw.ai/concepts/agent; https://docs.openclaw.ai/tools/exec; https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/tools/edit.ts'),
    'workbuddy': ('WorkBuddy with Desktop Commander MCP',
                  'https://www.codebuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/MCP-Guide; ' + DC_SOURCE),
}


def operations(runtime):
    cd_run = 'cd "{workspace}" && ' + RUN
    if runtime == 'claude-code':
        return [('Read', {'file_path': TARGET}),
                ('Edit', {'file_path': TARGET, 'old_string': 'return value + 1', 'new_string': 'return value * 2'}),
                ('Bash', {'command': cd_run, 'description': 'Run calculator checks'})]
    if runtime in ('claude-desktop', 'workbuddy'):
        return [('read_file', {'path': TARGET}),
                ('edit_block', {'file_path': TARGET, 'old_string': 'return value + 1', 'new_string': 'return value * 2'}),
                ('start_process', {'command': cd_run, 'timeout_ms': 10000})]
    if runtime == 'cursor':
        return [('Read', {'path': TARGET}), ('ApplyPatch', PATCH),
                ('Shell', {'command': RUN, 'working_directory': '{workspace}'})]
    if runtime == 'codex':
        return [('exec_command', {'cmd': "sed -n '1,80p' src/calculator.py", 'workdir': '{workspace}'}),
                ('apply_patch', PATCH), ('exec_command', {'cmd': RUN, 'workdir': '{workspace}'})]
    if runtime == 'opencode':
        return [('read', {'path': TARGET}),
                ('edit', {'path': TARGET, 'oldString': 'return value + 1', 'newString': 'return value * 2'}),
                ('shell', {'command': RUN, 'workdir': '{workspace}'})]
    if runtime == 'hermes':
        return [('read_file', {'path': TARGET}),
                ('patch', {'path': TARGET, 'old_string': 'return value + 1', 'new_string': 'return value * 2'}),
                ('terminal', {'command': cd_run})]
    if runtime == 'openclaw':
        return [('read', {'path': TARGET}),
                ('edit', {'path': TARGET, 'edits': [{'oldText': 'return value + 1', 'newText': 'return value * 2'}]}),
                ('exec', {'command': RUN, 'workdir': '{workspace}'})]
    raise ValueError('Unknown workflow profile')


def add_workflows(policy, check, binding):
    for runtime, (profile, sources) in PROFILES.items():
        records, layout, steps, annotations = [], [], [], []
        for sequence, (name, arguments) in enumerate(operations(runtime)):
            identity = f'call_calculator_{sequence}'
            output = BEFORE if sequence == 0 else 'Updated src/calculator.py\n' if sequence == 1 else 'calculator checks passed\n'
            i = len(records)
            base = f'/records/{i}'
            if runtime in ('claude-code', 'claude-desktop', 'cursor'):
                call = {'type': 'tool_use', 'id': identity, 'name': name, 'input': arguments}
                result = {'type': 'tool_result', 'tool_use_id': identity, 'content': output}
                if runtime == 'cursor':
                    records.extend([{'role': 'assistant', 'message': {'content': [call]}},
                                    {'role': 'user', 'message': {'content': [result]}}])
                    preserves = [[f'/records/{j}/role'] for j in (i, i + 1)]
                else:
                    records.extend([{'type': 'assistant', 'message': {'role': 'assistant', 'content': [call]}},
                                    {'type': 'user', 'message': {'role': 'user', 'content': [result]}}])
                    preserves = [[f'/records/{j}/type', f'/records/{j}/message/role'] for j in (i, i + 1)]
                call_root, result_root = base + '/message/content/0', f'/records/{i + 1}/message/content/0'
                call_fields = {k: binding(call_root + '/' + k) for k in ('id', 'name', 'input')}
                result_fields = {k: binding(result_root + '/' + k) for k in ('tool_use_id', 'content')}
            elif runtime == 'codex':
                freeform = isinstance(arguments, str)
                payload = {'type': 'custom_tool_call' if freeform else 'function_call', 'call_id': identity, 'name': name,
                           'input' if freeform else 'arguments': arguments if freeform else json.dumps(arguments, sort_keys=True)}
                records.extend([{'type': 'response_item', 'payload': payload},
                    {'type': 'response_item', 'payload': {'type': 'custom_tool_call_output' if freeform else 'function_call_output',
                                                        'call_id': identity, 'output': output}}])
                preserves = [[f'/records/{j}/type', f'/records/{j}/payload/type'] for j in (i, i + 1)]
                call_fields = {'id': binding(base + '/payload/call_id'), 'name': binding(base + '/payload/name'),
                               'input': binding(base + ('/payload/input' if freeform else '/payload/arguments'), not freeform)}
                result_fields = {'tool_use_id': binding(f'/records/{i + 1}/payload/call_id'),
                                 'content': binding(f'/records/{i + 1}/payload/output')}
            elif runtime == 'opencode':
                item = {'type': 'tool', 'callID': identity, 'tool': name,
                        'state': {'status': 'completed', 'input': arguments, 'output': output}}
                records.append({'kind': 'session_message', 'type': 'assistant', 'id': f'message_{sequence}',
                                'session_id': 'synthetic-calculator', 'data': {'content': [item]}})
                preserves = [[base + '/kind', base + '/type']]
                call_root = base + '/data/content/0'
                call_fields = {'id': binding(call_root + '/callID'), 'name': binding(call_root + '/tool'),
                               'input': binding(call_root + '/state/input')}
                result_fields = {'tool_use_id': binding(call_root + '/callID'), 'content': binding(call_root + '/state/output')}
            elif runtime == 'hermes':
                records.extend([{'type': 'hermes_message', 'data': {'role': 'assistant', 'content': None,
                    'tool_calls': [{'id': identity, 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(arguments, sort_keys=True)}}]}},
                    {'type': 'hermes_message', 'data': {'role': 'tool', 'tool_call_id': identity, 'content': output}}])
                preserves = [[f'/records/{j}/type', f'/records/{j}/data/role'] for j in (i, i + 1)]
                call_root = base + '/data/tool_calls/0'
                call_fields = {'id': binding(call_root + '/id'), 'name': binding(call_root + '/function/name'),
                               'input': binding(call_root + '/function/arguments', True)}
                result_fields = {'tool_use_id': binding(f'/records/{i + 1}/data/tool_call_id'),
                                 'content': binding(f'/records/{i + 1}/data/content')}
            elif runtime == 'openclaw':
                records.extend([{'type': 'message', 'id': f'event_{i}', 'message': {'role': 'assistant', 'content': [
                    {'type': 'toolCall', 'id': identity, 'name': name, 'arguments': arguments}]}},
                    {'type': 'message', 'id': f'event_{i + 1}', 'message': {'role': 'toolResult', 'toolCallId': identity, 'content': output}}])
                preserves = [[f'/records/{j}/type', f'/records/{j}/message/role'] for j in (i, i + 1)]
                call_root = base + '/message/content/0'
                call_fields = {'id': binding(call_root + '/id'), 'name': binding(call_root + '/name'), 'input': binding(call_root + '/arguments')}
                result_fields = {'tool_use_id': binding(f'/records/{i + 1}/message/toolCallId'),
                                 'content': binding(f'/records/{i + 1}/message/content')}
            elif runtime == 'workbuddy':
                records.extend([{'type': 'function_call', 'id': f'event_{i}', 'callId': identity, 'name': name,
                                 'arguments': json.dumps(arguments, sort_keys=True)},
                                {'type': 'function_call_result', 'id': f'event_{i + 1}', 'callId': identity,
                                 'status': 'completed', 'output': output}])
                preserves = [[f'/records/{j}/type'] for j in (i, i + 1)]
                call_fields = {'id': binding(base + '/callId'), 'name': binding(base + '/name'), 'input': binding(base + '/arguments', True)}
                result_fields = {'tool_use_id': binding(f'/records/{i + 1}/callId'), 'content': binding(f'/records/{i + 1}/output')}
            call_block = {'type': 'tool_use', 'fields': call_fields}
            result_block = {'type': 'tool_result', 'fields': result_fields}
            layout.append({'role': 'assistant', 'preserve': preserves[0], 'blocks': [call_block, result_block] if runtime == 'opencode' else [call_block]})
            if runtime != 'opencode':
                layout.append({'role': 'user', 'preserve': preserves[1], 'blocks': [result_block]})
            public_call = base + '/message/content/0'
            public_result = base + '/message/content/1/content' if runtime == 'opencode' else f'/records/{i + 1}/message/content/0/content'
            steps.append({'operation': ('read', 'edit', 'test')[sequence], 'name': call_fields['name'], 'arguments': call_fields['input'],
                          'result': result_fields['content']})
            annotations.append((name, arguments, output, call_fields, result_fields, public_call, public_result))
        for mode in ('encrypted_public', 'plain'):
            checks = []
            for name, arguments, output, call_fields, result_fields, public_call, public_result in annotations:
                input_pointer = call_fields['input']['pointer']
                original = json.dumps(arguments, sort_keys=True) if call_fields['input'].get('encoding') == 'json' else arguments
                checks.append(check(call_fields['name']['pointer'] if mode == 'plain' else public_call + '/name',
                                    'keep', name, input_pointer=call_fields['name']['pointer']))
                checks.append(check(result_fields['content']['pointer'] if mode == 'plain' else public_result,
                                    'keep', output, input_pointer=result_fields['content']['pointer']))
                if mode == 'plain':
                    checks.append(check(input_pointer, 'keep', original))
                elif isinstance(arguments, str):
                    checks.append(check(public_call + '/input', 'keep', arguments, input_pointer=input_pointer))
                else:
                    for field, value in arguments.items():
                        source = input_pointer if isinstance(original, str) else input_pointer + '/' + field
                        source_value = original if isinstance(original, str) else value
                        target = public_call + '/input/' + field
                        if isinstance(value, str) and '{workspace}' in value:
                            pattern = r'<(?:workspace|private-file-[1-9][0-9]*)>'.join(re.escape(part) for part in value.split('{workspace}'))
                            checks.append(check(target, 'rewrite', source_value, input_pointer=source, replacement_pattern=pattern))
                        elif isinstance(original, str):
                            checks.append(check(target, 'rewrite', original, value, input_pointer=source))
                        else:
                            checks.append(check(target, 'keep', value, input_pointer=source))
            policy(f'workflow-{runtime}-calculator-{mode.replace("_", "-")}', records, checks,
                   'Preserve the read/edit/test sequence, patch or replacement contents, test result, and tool associations; alias local paths only in the public view.',
                   config={'include': ['src/**', 'tests/**']}, runtime=runtime, record_layout=layout, mode=mode,
                   coverage_kind='tool-workflow', tool_profile=profile, tool_sources=sources,
                   source='Constructed reference-backed trace, not a captured agent session. Tool contracts checked on 2026-11-06.',
                   scenario='The user asks to fix double(3) returning 4. The agent reads the implementation, changes addition to multiplication, and runs the existing calculator checks.',
                   expectation_basis='docs/privacy-benchmark.md; docs/privacy-plain-publication.md',
                   workspace_files={'src/calculator.py': BEFORE, 'tests/check_calculator.py': TEST},
                   workflow={'steps': steps, 'target': 'src/calculator.py', 'after': AFTER, 'test': 'tests/check_calculator.py'},
                   validity=[{'format': 'agent-workflow', 'pointer': ''}])
