"""Construct synthetic conversations and independent expectations from production contracts."""
import hashlib
import json
from pathlib import Path

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
    for pointer, text in strings(value):
        ranges = []
        for sensitive in protected.get(pointer, []):
            start = 0
            while (start := text.find(sensitive, start)) >= 0:
                ranges.append((start, start + len(sensitive)))
                start += len(sensitive)
        cursor = 0
        for start, end in sorted(set(ranges)):
            assert start >= cursor
            if start > cursor:
                checks.append({'pointer': pointer, 'start': cursor, 'end': start, 'expect': 'keep'})
            checks.append({'pointer': pointer, 'start': start, 'end': end, 'expect': 'protect'})
            cursor = end
        if cursor < len(text):
            checks.append({'pointer': pointer, 'start': cursor, 'end': len(text), 'expect': 'keep'})
    CASES.append(case)
    ANNOTATIONS[name] = checks


def policy(name, records, checks, description, config=None, metadata=None, **options):
    CASES.append({'id': name, 'layer': 'policy', 'input': {'records': records, 'metadata': metadata or {}},
                  'policy': {'version': 1, 'include': ['src/**'], **(config or {})}, 'description': description,
                  'mode': options.pop('mode', 'encrypted_public'),
                  'source': 'Reconstructed from privacy_publication.rs and privacy/repository.rs contracts; no historical session data',
                  **options})
    ANNOTATIONS[name] = checks


def message(text):
    return {'type': 'user', 'message': {'role': 'user', 'content': text}}


def tools(path, body, identity):
    return [{'type': 'assistant', 'message': {'role': 'assistant', 'content': [
                {'type': 'tool_use', 'id': identity, 'name': 'Read', 'input': {'file_path': path}}]}},
            {'type': 'user', 'message': {'role': 'user', 'content': [
                {'type': 'tool_result', 'tool_use_id': identity, 'content': body}]}}]


def check(pointer, expect, value, replacement=None):
    result = {'pointer': pointer, 'expect': expect, 'value': value}
    if replacement is not None:
        result['replacement'] = replacement
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

path_pointer = '/records/0/message/content/0/input/file_path'
result_pointer = '/records/1/message/content/0/content'
for name, path, excluded, config in [
    ('default-file-exclusion', '{workspace}/.env', True, {}),
    ('user-file-exclusion', '{workspace}/private.txt', True, {'exclude': ['private.txt']}),
    ('ordinary-file', '{workspace}/src/main.rs', False, {}),
]:
    policy(name, tools(path, 'CONSTRUCTED_FILE_BODY', 'call-example'),
           [check('/records', 'hide' if excluded else 'keep', 'CONSTRUCTED_FILE_BODY'),
            check(path_pointer, 'hide' if excluded else 'rewrite', path,
                  '<workspace>/src/main.rs' if not excluded else None)],
           'File access policy follows the call through to its result; public source content stays readable.', config)
policy('path-mention', [message('Mentioning {workspace}/.env does not read it. Keep this explanation.')],
       [check('/records', 'keep', 'does not read it. Keep this explanation.')],
       'Mentioning an excluded path does not hide the surrounding message.')
policy('literal-replacement', [message('Hello bench-person and ordinary neighbor.')],
       [check('/records/0/message/content', 'rewrite', 'bench-person', 'public-person'),
        check('/records/0/message/content', 'keep', 'ordinary neighbor.')],
       'Literal substitution preserves adjacent conversation.',
       {'replacements': [{'pattern': 'bench-person', 'replacement': 'public-person'}]})
policy('regex-replacement', [message('Ticket BENCH-123 and ordinary neighbor.')],
       [check('/records/0/message/content', 'rewrite', 'BENCH-123', 'TICKET'),
        check('/records/0/message/content', 'keep', 'ordinary neighbor.')],
       'Regex substitution changes only the selected content.',
       {'replacements': [{'pattern': 'BENCH-[0-9]+', 'replacement': 'TICKET', 'regex': True}]})
for mode in ['omit', 'metadata', 'original']:
    policy('attachment-' + mode,
           [{'type': 'user', 'message': {'role': 'user', 'content': [
               {'type': 'text', 'text': 'Keep the adjacent message.'},
               {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': 'U1lOVEhFVElDX0lNQUdF'}}]}}],
           [check('/records', 'keep', 'Keep the adjacent message.'),
            check('/records', 'keep' if mode == 'original' else 'hide', 'U1lOVEhFVElDX0lNQUdF')],
           'Attachment mode controls the synthetic payload without hiding adjacent text.', {'attachments': mode})
for mode in ['minimal', 'project']:
    policy('metadata-' + mode, [message('Keep the conversation.')],
           [check('/records', 'keep', 'Keep the conversation.'), check('/metadata', 'hide', '/bench/private-user'),
            check('/metadata', 'keep' if mode == 'project' else 'hide', 'https://example.invalid/team/repo')],
           'Metadata mode limits project coordinates and never exposes the original device path.',
           {'metadata': mode}, {'cwd': '/bench/private-user', 'cwd_state': {'origin': 'https://example.invalid/team/repo', 'worktree': 'clean'}})

secret('split-context', PASSWORD + '" and a safe suffix.', {'': [PASSWORD]},
       'A credential whose assignment precedes the chunk retains its context.', context='password = "')
secret('stream-boundary', 'Before\npassword = "' + PASSWORD + '"\n' + 'Keep this public line.\n' * 8,
       {'': [PASSWORD]}, 'A credential split across input chunks cannot escape through the released prefix.',
       stream_cuts=[27, 43, 90])
secret('entropy-source-identifiers', 'const digest = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";\n'
       'Asset: https://assets.example.invalid/font/UcCO3FwrK3iLTeHuS_fvQtMwCp50KnMw2boKoduKmMEVuLyfAZ9hiA.woff2', {},
       'Typed digests and resource URLs retain their source evidence under entropy opt-in.', entropy=True)
policy('plain-file-content' , tools('{workspace}/.env', 'PLAIN_FILE_BODY', 'plain-call'),
       [check('/records', 'keep', 'PLAIN_FILE_BODY'), check('/records', 'keep', '{workspace}/.env')],
       'Plain capture protection does not apply public-view file exclusions.', mode='plain')
policy('privacy-export-file', tools('{workspace}/.env', 'EXPORT_PRIVATE_BODY', 'export-call'),
       [check('/records', 'hide', 'EXPORT_PRIVATE_BODY')],
       'Privacy export shares the frozen projection used by the encrypted public view.', mode='privacy_export')
policy('memory-allowlist', [], [check('/decisions/0/action', 'allow', 'allowed'),
                              check('/decisions/1/action', 'exclude', 'excluded')],
       'Memory is opt-in and sensitive paths remain excluded even with an allowlist.',
       {'memory_allow': ['notes.md', '.env']}, mode='memory', paths=['notes.md', '.env'])


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


if __name__ == '__main__':
    write(ROOT / 'cases/synthetic.json', CASES)
    write(ROOT / 'annotations/synthetic.json', ANNOTATIONS)
    write(ROOT / 'profiles/default.json', {'version': 1, 'case_ids': [c['id'] for c in CASES if not c.get('entropy') and c.get('mode') != 'memory']})
    write(ROOT / 'profiles/entropy.json', {'version': 1, 'case_ids': ['bare-entropy-default', 'bare-entropy-opt-in', 'entropy-source-identifiers']})
    write(ROOT / 'profiles/all.json', {'version': 1, 'case_ids': [c['id'] for c in CASES]})
    paths = sorted([ROOT / 'generate.py', *ROOT.glob('cases/*.json'), *ROOT.glob('annotations/*.json'), *ROOT.glob('profiles/*.json')])
    hashes = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}
    write(ROOT / 'manifest.json', {'schema_version': 1, 'dataset_version': '1.0.1', 'annotation_version': 2,
                                  'offset_unit': 'Unicode code points in decoded JSON strings',
                                  'seed': None, 'files': hashes,
                                  'dataset_sha256': hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest(),
                                  'provenance': 'Reconstructed synthetic contracts; historical private audit artifacts were not imported.'})
