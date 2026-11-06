"""Build fixed synthetic boundary and workflow cases with independently authored expectations."""
import hashlib
import json
from pathlib import Path
import runpy

ROOT = Path(__file__).resolve().parent
CASES = []
ANNOTATIONS = {}
TOKEN = 'ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr'
PASSWORD = 'c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0'
OPAQUE = 'Bf7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr'


def strings(value, pointer=''):
    if isinstance(value, str):
        yield pointer, value
    elif isinstance(value, (list, dict)):
        for key, item in (enumerate(value) if isinstance(value, list) else value.items()):
            yield from strings(item, pointer + '/' + str(key).replace('~', '~0').replace('/', '~1'))


def secret(name, value, protected, description, **options):
    case = {'id': name, 'layer': 'secret', 'input': value, 'description': description,
            'source': 'Reconstructed from privacy/policy.rs and privacy/projector.rs contracts; synthetic values only',
            **options}
    checks = []
    locations = dict(strings(value))
    if set(protected) - locations.keys():
        raise ValueError(f'{name}: protected pointer does not identify a string')
    for pointer, text in locations.items():
        ranges = []
        for selection in protected.get(pointer, []):
            spec = {'value': selection} if isinstance(selection, str) else selection
            if not isinstance(spec, dict) or set(spec) - {'value', 'count', 'occurrences', 'start', 'end'}:
                raise ValueError(f'{name}: invalid protected selection at {pointer}')
            sensitive = spec['value']
            if not isinstance(sensitive, str) or not sensitive:
                raise ValueError(f'{name}: protected value must be nonempty text')
            if 'start' in spec or 'end' in spec:
                start, end = spec['start'], spec['end']
                if (set(spec) != {'value', 'start', 'end'} or type(start) is not int or type(end) is not int
                        or not 0 <= start < end <= len(text) or text[start:end] != sensitive):
                    raise ValueError(f'{name}: invalid explicit range at {pointer}')
                ranges.append((start, end))
                continue
            matches = []
            start = 0
            while (start := text.find(sensitive, start)) >= 0:
                matches.append((start, start + len(sensitive)))
                start += len(sensitive)
            if not matches or ('count' in spec and (type(spec['count']) is not int or spec['count'] != len(matches))):
                raise ValueError(f'{name}: protected occurrence count mismatch at {pointer}')
            selected = spec.get('occurrences', list(range(len(matches))))
            if (not isinstance(selected, list) or not selected or len(set(selected)) != len(selected)
                    or any(type(i) is not int or not 0 <= i < len(matches) for i in selected)):
                raise ValueError(f'{name}: invalid occurrence selection at {pointer}')
            ranges.extend(matches[i] for i in selected)
        cursor = 0
        for start, end in sorted(ranges):
            if start < cursor:
                raise ValueError(f'{name}: overlapping protected ranges at {pointer}')
            if start > cursor:
                checks.append({'pointer': pointer, 'start': cursor, 'end': start, 'expect': 'keep'})
            checks.append({'pointer': pointer, 'start': start, 'end': end, 'expect': 'protect'})
            cursor = end
        if cursor < len(text):
            checks.append({'pointer': pointer, 'start': cursor, 'end': len(text), 'expect': 'keep'})
    CASES.append(case)
    ANNOTATIONS[name] = checks


def policy(name, records, checks, description, config=None, metadata=None, **options):
    mode = options.pop('mode', 'encrypted_public')
    runtime = options.pop('runtime', 'claude-code')
    layout = options.pop('record_layout', None)
    if mode != 'memory':
        options.update(runtime=runtime, record_layout=layout if layout is not None else message_layout(records))
    CASES.append({'id': name, 'layer': 'policy', 'input': {'records': records, 'metadata': metadata or {}},
                  'policy': {'version': 1, 'include': ['src/**'], **(config or {})}, 'description': description,
                  'mode': mode,
                  'source': 'Reconstructed from privacy_publication.rs and privacy/repository.rs contracts; no historical session data',
                  **options})
    ANNOTATIONS[name] = checks


def binding(pointer, encoded=False):
    return {'pointer': pointer, **({'encoding': 'json'} if encoded else {})}


def message_layout(records):
    layout = []
    for i, record in enumerate(records):
        content = record['message']['content']
        prefix = f'/records/{i}/message/content'
        blocks = ([{'type': 'text', 'fields': {'text': binding(prefix)}}] if isinstance(content, str) else
                  [{'type': block['type'], 'fields': {k: binding(f'{prefix}/{j}/{k}') for k in block if k != 'type'}}
                   for j, block in enumerate(content)])
        layout.append({'role': record['message']['role'], 'blocks': blocks,
                       'preserve': [f'/records/{i}/type', f'/records/{i}/message/role']})
    return layout


def message(text):
    return {'type': 'user', 'message': {'role': 'user', 'content': text}}


def tools(path, body, identity):
    return [{'type': 'assistant', 'message': {'role': 'assistant', 'content': [
                {'type': 'tool_use', 'id': identity, 'name': 'Read', 'input': {'file_path': path}}]}},
            {'type': 'user', 'message': {'role': 'user', 'content': [
                {'type': 'tool_result', 'tool_use_id': identity, 'content': body}]}}]


def check(pointer, expect, value, replacement=None, *, input_pointer=None, replacement_pattern=None):
    result = {'pointer': pointer, 'expect': expect, 'value': value}
    if expect not in ('allow', 'exclude'):
        result['input_pointer'] = pointer if input_pointer is None else input_pointer
    if replacement is not None:
        result['replacement'] = replacement
    if replacement_pattern is not None:
        result['replacement_pattern'] = replacement_pattern
    return result


secret('provider-repeated', f'First {TOKEN}; second {TOKEN}. Keep this explanation.',
       {'': [TOKEN]}, 'Each repeated provider credential is protected without deleting its neighbors.')
secret('multiple-credentials', f'password="{PASSWORD}"; Authorization: Bearer {OPAQUE}\nKeep this note.',
       {'': [PASSWORD, OPAQUE]}, 'Missing either credential counts as its own false negative.')
secret('context-and-references', {'messages': [message('type Config = { password: string; api_key?: string }; password = settings.password; token = process.env.API_KEY'),
                                            message(f'password = "{PASSWORD}"')]},
       {'/messages/1/message/content': [PASSWORD]}, 'References remain readable beside a real password assignment.')
secret('structured-fields', {'api_key': TOKEN, 'file_token': OPAQUE, 'count': 7, 'enabled': True,
                             'message': 'Keep "quoted" text and a newline\nhere.'},
       {'/api_key': [TOKEN]}, 'Decoded credentials are protected and document tokens, types and escapes survive.')
secret('protocol-id', {'type': 'tool_use', 'id': TOKEN, 'name': 'Read', 'input': {'note': TOKEN}},
       {'/input/note': [TOKEN]}, 'Native protocol identifiers keep identity while ordinary content is protected.')
secret('normal-placeholders', 'password = "YOUR_PASSWORD"; https://example.invalid/docs; API_KEY = os.getenv("API_KEY")',
       {}, 'Document placeholders and configuration references are not actual credentials.')
secret('bare-entropy-default', 'Trace identifier: ' + OPAQUE, {}, 'Bare entropy remains readable without opt-in.')
secret('bare-entropy-opt-in', 'Trace identifier: ' + OPAQUE, {'': [OPAQUE]},
       'Entropy opt-in protects a bare synthetic high-entropy value.', entropy=True)
secret('explicit-block', 'Keep prefix confidential-demo-value suffix.', {'': ['confidential-demo-value']},
       'An explicit rule protects an otherwise ordinary value.', blocks=['confidential-demo-value'])
secret('allow-default', f'Example {TOKEN} remains allowed.', {},
       'An explicit allow overrides heuristic detection.', allows=[TOKEN])
secret('block-over-allow', f'Example {TOKEN} must be hidden.', {'': [TOKEN]},
       'An explicit block wins when the same value is allowed.', blocks=[TOKEN], allows=[TOKEN])
secret('url-password', f'Use https://bench-user:{PASSWORD}@example.invalid/path then continue.', {'': [PASSWORD]},
       'A URL credential is protected without removing the host and path.')
secret('private-key', 'Before\n-----BEGIN PRIVATE KEY-----\n' + OPAQUE + '\n-----END PRIVATE KEY-----\nAfter',
       {'': ['-----BEGIN PRIVATE KEY-----\n' + OPAQUE + '\n-----END PRIVATE KEY-----']},
       'The complete synthetic private-key block is protected.')

call_pointer = '/records/0/message/content/0'
path_pointer = call_pointer + '/input/file_path'
result_pointer = '/records/1/message/content/0/content'
text_pointer = '/records/0/message/content/0/text'
text_input = '/records/0/message/content'
file_notice = '[File content excluded by policy]'


def file_checks(path, body, excluded, plain=False):
    if excluded:
        return [check(result_pointer, 'hide', body, file_notice),
                check(call_pointer + '/input', 'hide', path, {'notice': file_notice}, input_pointer=path_pointer),
                check(call_pointer + '/name', 'rewrite', 'Read', 'omitted')]
    return [check(result_pointer, 'keep', body),
            check(path_pointer, 'keep' if plain else 'rewrite', path,
                  None if plain else '<workspace>/src/main.rs'),
            check(call_pointer + '/name', 'keep', 'Read')]


for name, path, excluded, config in [
    ('default-file-exclusion', '{workspace}/.env', True, {}),
    ('user-file-exclusion', '{workspace}/private.txt', True, {'exclude': ['private.txt']}),
    ('ordinary-file', '{workspace}/src/main.rs', False, {}),
]:
    policy(name, tools(path, 'CONSTRUCTED_FILE_BODY', 'call-example'),
           file_checks(path, 'CONSTRUCTED_FILE_BODY', excluded),
           'File access policy follows the call through to its result; public source content stays readable.', config)
mention = 'Mentioning {workspace}/.env does not read it. Keep this explanation.'
policy('path-mention', [message(mention)],
       [check(text_pointer, 'rewrite', mention, input_pointer=text_input,
              replacement_pattern=r'Mentioning <private-file-[1-9][0-9]*> does not read it\. Keep this explanation\.')],
       'Mentioning an excluded path aliases the path and preserves the complete surrounding message.')
policy('literal-replacement', [message('Hello bench-person and ordinary neighbor.')],
       [check(text_pointer, 'rewrite', 'Hello bench-person and ordinary neighbor.',
              'Hello public-person and ordinary neighbor.', input_pointer=text_input)],
       'Literal substitution preserves adjacent conversation.',
       {'replacements': [{'pattern': 'bench-person', 'replacement': 'public-person'}]})
policy('regex-replacement', [message('Ticket BENCH-123 and ordinary neighbor.')],
       [check(text_pointer, 'rewrite', 'Ticket BENCH-123 and ordinary neighbor.',
              'Ticket TICKET and ordinary neighbor.', input_pointer=text_input)],
       'Regex substitution changes only the selected content.',
       {'replacements': [{'pattern': 'BENCH-[0-9]+', 'replacement': 'TICKET', 'regex': True}]})
for mode in ['omit', 'metadata', 'original']:
    attachment = {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': 'U1lOVEhFVElDX0lNQUdF'}}
    attachment_pointer = '/records/0/message/content/1'
    checks = [check('/records/0/message/content/0', 'keep', {'type': 'text', 'text': 'Keep the adjacent message.'})]
    if mode == 'original':
        checks.append(check(attachment_pointer, 'keep', attachment))
    else:
        notice = ('[Image omitted by policy]' if mode == 'omit' else
                  '[Attachment body omitted; metadata retained: {"media_type":"image/png","type":"image"}]')
        checks.append(check(attachment_pointer, 'hide', attachment['source']['data'],
                            {'type': 'text', 'text': notice}, input_pointer=attachment_pointer + '/source/data'))
    policy('attachment-' + mode,
           [{'type': 'user', 'message': {'role': 'user', 'content': [
               {'type': 'text', 'text': 'Keep the adjacent message.'}, attachment]}}], checks,
           'Attachment mode controls the synthetic payload and retains the required notice or metadata.', {'attachments': mode})
for mode in ['minimal', 'project']:
    origin = 'https://example.invalid/team/repo'
    policy('metadata-' + mode, [message('Keep the conversation.')],
           [check(text_pointer, 'keep', 'Keep the conversation.', input_pointer=text_input),
            check('/metadata/cwd', 'remove', '/bench/private-user'),
            check('/metadata/workspace', 'hide', '/bench/private-user', input_pointer='/metadata/cwd',
                  replacement_pattern=r'<private-file-[1-9][0-9]*>'),
            check('/metadata/origin', 'keep' if mode == 'project' else 'remove', origin,
                  input_pointer='/metadata/cwd_state/origin'),
            check('/metadata/cwd_state', 'remove', {'origin': origin, 'worktree': 'clean'})],
           'Metadata mode limits project coordinates and never exposes the original device path.',
           {'metadata': mode}, {'cwd': '/bench/private-user', 'cwd_state': {'origin': origin, 'worktree': 'clean'}})

secret('split-context', PASSWORD + '" and a safe suffix.', {'': [PASSWORD]},
       'A credential whose assignment precedes the chunk retains its context.', context='password = "')
secret('stream-boundary', 'Before\npassword = "' + PASSWORD + '"\n' + 'Keep this public line.\n' * 8,
       {'': [PASSWORD]}, 'A credential split across input chunks cannot escape through the released prefix.',
       stream_cuts=[27, 43, 90])
secret('entropy-source-identifiers', 'const digest = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";\n'
       'Asset: https://assets.example.invalid/font/UcCO3FwrK3iLTeHuS_fvQtMwCp50KnMw2boKoduKmMEVuLyfAZ9hiA.woff2', {},
       'Typed digests and resource URLs retain their source evidence under entropy opt-in.', entropy=True)
policy('plain-file-content' , tools('{workspace}/.env', 'PLAIN_FILE_BODY', 'plain-call'),
       file_checks('{workspace}/.env', 'PLAIN_FILE_BODY', False, plain=True),
       'Plain capture protection does not apply public-view file exclusions.', mode='plain')
policy('privacy-export-file', tools('{workspace}/.env', 'EXPORT_PRIVATE_BODY', 'export-call'),
       file_checks('{workspace}/.env', 'EXPORT_PRIVATE_BODY', True),
       'Privacy export shares the frozen projection used by the encrypted public view.', mode='privacy_export')
policy('memory-allowlist', [], [check('/decisions/0/action', 'allow', 'allowed'),
                              check('/decisions/1/action', 'exclude', 'excluded')],
       'Memory is opt-in and sensitive paths remain excluded even with an allowlist.',
       {'memory_allow': ['notes.md', '.env']}, mode='memory', paths=['notes.md', '.env'])


def native_tool_fixture(runtime):
    records, layout = [], []
    entries = [('call', 'private'), ('call', 'public'), ('result', 'public'), ('result', 'private')]
    if runtime == 'opencode':
        entries = [('combined', 'private'), ('combined', 'public')]
    for i, (kind, scope) in enumerate(entries):
        identity = 'call-' + scope
        name = 'Read' if scope == 'private' else 'ReadPublic'
        arguments = {'file_path': '{workspace}/src/' + scope + '.rs'}
        body = 'SYNTHETIC_' + scope.upper() + '_BODY'
        encoded = json.dumps(arguments, sort_keys=True)
        role = 'user' if kind == 'result' else 'assistant'
        prefix = f'/records/{i}'
        if runtime in ('claude-code', 'claude-desktop', 'cursor'):
            block = ({'type': 'tool_use', 'id': identity, 'name': name, 'input': arguments} if kind == 'call' else
                     {'type': 'tool_result', 'tool_use_id': identity, 'content': body})
            record = {'type': role, 'message': {'role': role, 'content': [block]}}
            blocks = message_layout([record])[0]['blocks']
            for source in blocks[0]['fields'].values():
                source['pointer'] = source['pointer'].replace('/records/0/', prefix + '/', 1)
            if runtime == 'cursor':
                record = {'role': role, 'message': {'content': [block]}}
        elif runtime == 'codex':
            payload = ({'type': 'function_call', 'call_id': identity, 'name': name, 'arguments': encoded} if kind == 'call' else
                       {'type': 'function_call_output', 'call_id': identity, 'output': body})
            record = {'type': 'response_item', 'payload': payload}
            fields = ({'id': binding(prefix + '/payload/call_id'), 'name': binding(prefix + '/payload/name'),
                       'input': binding(prefix + '/payload/arguments', True)} if kind == 'call' else
                      {'tool_use_id': binding(prefix + '/payload/call_id'), 'content': binding(prefix + '/payload/output')})
            blocks = [{'type': 'tool_use' if kind == 'call' else 'tool_result', 'fields': fields}]
        elif runtime == 'opencode':
            item = {'type': 'tool', 'callID': identity, 'tool': name,
                    'state': {'status': 'completed', 'input': arguments, 'output': body}}
            record = {'kind': 'session_message', 'type': 'assistant', 'id': 'message-' + scope,
                      'session_id': 'synthetic-session', 'data': {'content': [item]}}
            base = prefix + '/data/content/0'
            blocks = [{'type': 'tool_use', 'fields': {'id': binding(base + '/callID'), 'name': binding(base + '/tool'),
                       'input': binding(base + '/state/input')}},
                      {'type': 'tool_result', 'fields': {'tool_use_id': binding(base + '/callID'),
                       'content': binding(base + '/state/output')}}]
        elif runtime == 'hermes':
            data = ({'role': 'assistant', 'content': None, 'tool_calls': [
                        {'id': identity, 'type': 'function', 'function': {'name': name, 'arguments': encoded}}]} if kind == 'call' else
                    {'role': 'tool', 'tool_call_id': identity, 'content': body})
            record = {'type': 'hermes_message', 'data': data}
            base = prefix + '/data/tool_calls/0'
            fields = ({'id': binding(base + '/id'), 'name': binding(base + '/function/name'),
                       'input': binding(base + '/function/arguments', True)} if kind == 'call' else
                      {'tool_use_id': binding(prefix + '/data/tool_call_id'), 'content': binding(prefix + '/data/content')})
            blocks = [{'type': 'tool_use' if kind == 'call' else 'tool_result', 'fields': fields}]
        elif runtime == 'openclaw':
            message = ({'role': 'assistant', 'content': [
                        {'type': 'toolCall', 'id': identity, 'name': name, 'arguments': arguments}]} if kind == 'call' else
                       {'role': 'toolResult', 'toolCallId': identity, 'content': body})
            record = {'type': 'message', 'id': f'event-{i}', 'message': message}
            base = prefix + '/message/content/0'
            fields = ({'id': binding(base + '/id'), 'name': binding(base + '/name'), 'input': binding(base + '/arguments')}
                      if kind == 'call' else {'tool_use_id': binding(prefix + '/message/toolCallId'),
                                              'content': binding(prefix + '/message/content')})
            blocks = [{'type': 'tool_use' if kind == 'call' else 'tool_result', 'fields': fields}]
        elif runtime == 'workbuddy':
            record = ({'type': 'function_call', 'callId': identity, 'name': name, 'arguments': encoded} if kind == 'call' else
                      {'type': 'function_call_result', 'callId': identity, 'output': body})
            fields = ({'id': binding(prefix + '/callId'), 'name': binding(prefix + '/name'),
                       'input': binding(prefix + '/arguments', True)} if kind == 'call' else
                      {'tool_use_id': binding(prefix + '/callId'), 'content': binding(prefix + '/output')})
            blocks = [{'type': 'tool_use' if kind == 'call' else 'tool_result', 'fields': fields}]
        else:
            raise ValueError('Unknown synthetic runtime')
        protocol_fields = {
            'claude-code': ('type', 'message/role'), 'claude-desktop': ('type', 'message/role'),
            'cursor': ('role',), 'codex': ('type', 'payload/type'), 'opencode': ('kind', 'type'),
            'hermes': ('type', 'data/role'), 'openclaw': ('type', 'message/role'), 'workbuddy': ('type',),
        }
        records.append(record)
        layout.append({'role': role, 'blocks': blocks, 'preserve': [prefix + '/' + field for field in protocol_fields[runtime]]})
    return records, layout


def source_value(records, pointer):
    value = {'records': records}
    for part in pointer.split('/')[1:]:
        value = value[int(part)] if isinstance(value, list) else value[part]
    return value


for runtime in ('claude-code', 'claude-desktop', 'cursor', 'codex', 'opencode', 'hermes', 'openclaw', 'workbuddy'):
    records, layout = native_tool_fixture(runtime)
    for mode in ('encrypted_public', 'plain'):
        checks = []
        for i, row in enumerate(layout):
            for j, block in enumerate(row['blocks']):
                fields = block['fields']
                identity = source_value(records, fields['id' if block['type'] == 'tool_use' else 'tool_use_id']['pointer'])
                hidden = mode != 'plain' and identity == 'call-private'
                for field in ('name', 'input') if block['type'] == 'tool_use' else ('content',):
                    source = fields[field]['pointer']
                    original = source_value(records, source)
                    target = source if mode == 'plain' else f'/records/{i}/message/content/{j}/{field}'
                    if hidden:
                        if field == 'name':
                            checks.append(check(target, 'rewrite', original, 'omitted', input_pointer=source))
                        elif field == 'input':
                            checks.append(check(target, 'rewrite', original, {'notice': '[Tool call omitted by policy]'}, input_pointer=source))
                        else:
                            checks.append(check(target, 'hide', original, '[Tool output omitted by policy]', input_pointer=source))
                    elif field == 'input' and mode != 'plain':
                        checks.append(check(target, 'rewrite', original, {'file_path': '<workspace>/src/public.rs'}, input_pointer=source))
                    else:
                        checks.append(check(target, 'keep', original, input_pointer=source))
        policy(f'native-{runtime}-{mode.replace("_", "-")}', records, checks,
               'Native tools retain their associations and ordinary results while source-specific omissions affect only the selected call.',
               {'tools': {'overrides': {runtime + ':Read': True}}}, runtime=runtime, record_layout=layout, mode=mode,
               coverage_kind='protocol-contract')


runpy.run_path(str(ROOT / 'scenarios.py'))['add_scenarios'](secret, policy, check, message, binding, ROOT)
runpy.run_path(str(ROOT / 'workflows.py'))['add_workflows'](policy, check, binding)
runpy.run_path(str(ROOT / 'incidents.py'))['add_incidents'](secret, policy, check, message, binding)


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


if __name__ == '__main__':
    catalog = runpy.run_path(str(ROOT / 'catalog.py'))['render_catalog'](CASES)
    (ROOT.parents[1] / 'docs/privacy-benchmark-cases.md').write_text(catalog)
    write(ROOT / 'cases/synthetic.json', CASES)
    write(ROOT / 'annotations/synthetic.json', ANNOTATIONS)
    write(ROOT / 'profiles/default.json', {'version': 1, 'case_ids': [c['id'] for c in CASES if not c.get('entropy') and c.get('mode') != 'memory']})
    write(ROOT / 'profiles/entropy.json', {'version': 1, 'case_ids': [c['id'] for c in CASES if c.get('entropy') or c['id'] == 'bare-entropy-default']})
    write(ROOT / 'profiles/all.json', {'version': 1, 'case_ids': [c['id'] for c in CASES]})
    paths = sorted([*ROOT.glob('*.py'), *ROOT.glob('fixtures/*'), *ROOT.glob('cases/*.json'), *ROOT.glob('annotations/*.json'), *ROOT.glob('profiles/*.json')])
    hashes = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}
    write(ROOT / 'manifest.json', {'schema_version': 1, 'dataset_version': '1.5.0', 'annotation_version': 3,
                                  'offset_unit': 'Unicode code points in decoded JSON strings',
                                  'seed': None, 'files': hashes,
                                  'dataset_sha256': hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest(),
                                  'provenance': 'Fixed boundary and workflow fixtures with independent expectations and public format references; no real session data.'})
