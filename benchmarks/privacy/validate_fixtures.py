"""Validate fixture syntax and encodings independently of privacy detection or benchmark verdicts."""
import argparse
import ast
import base64
import configparser
import hmac
import http.client
import io
import json
import os
import re
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
from urllib.parse import unquote, urlsplit
import zlib

ROOT = Path(__file__).resolve().parent


def require(condition):
    if not condition:
        raise ValueError('Fixture does not satisfy its declared format')


def at(value, path):
    for part in path.split('/')[1:]:
        part = part.replace('~1', '/').replace('~0', '~')
        value = value[int(part)] if isinstance(value, list) else value[part]
    return value


def command(args, content=None):
    if not shutil.which(args[0]):
        raise ValueError('Required format validator is unavailable: ' + args[0])
    completed = subprocess.run(args, input=content, capture_output=True, text=True, timeout=20)
    if completed.returncode:
        raise ValueError('Format validation failed: ' + args[0])
    return completed.stdout


def validate_png(encoded):
    data = base64.b64decode(encoded, validate=True)
    require(data[:8] == b'\x89PNG\r\n\x1a\n')
    offset, kinds, compressed = 8, [], b''
    while offset < len(data):
        length = struct.unpack('>I', data[offset:offset + 4])[0]
        kind = data[offset + 4:offset + 8]
        body = data[offset + 8:offset + 8 + length]
        crc = struct.unpack('>I', data[offset + 8 + length:offset + 12 + length])[0]
        require(zlib.crc32(kind + body) == crc)
        kinds.append(kind)
        if kind == b'IHDR':
            width, height, depth, color, compression, filtering, interlace = struct.unpack('>IIBBBBB', body)
            require(width > 0 and height > 0 and (depth, color, compression, filtering, interlace) == (8, 6, 0, 0, 0))
        elif kind == b'IDAT':
            compressed += body
        elif kind == b'IEND':
            require(length == 0 and offset + 12 == len(data))
        offset += length + 12
    require(offset == len(data) and kinds[0] == b'IHDR' and kinds[-1] == b'IEND')
    pixels = zlib.decompress(compressed)
    stride = 1 + width * 4
    require(len(pixels) == height * stride)
    require(all(pixels[row * stride] == 0 for row in range(height)))


def validate_workflow(case, external):
    workflow = case['workflow']
    require(workflow['target'] == 'src/calculator.py' and workflow['test'] == 'tests/check_calculator.py')
    steps = workflow['steps']
    require([s['operation'] for s in steps] == ['read', 'edit', 'test'])
    names, arguments, outputs = [], [], []
    for step in steps:
        names.append(at(case['input'], step['name']['pointer']))
        raw = at(case['input'], step['arguments']['pointer'])
        arguments.append(json.loads(raw) if step['arguments'].get('encoding') == 'json' else raw)
        outputs.append(at(case['input'], step['result']['pointer']))
    runtime = case['runtime']
    expected = {
        'claude-code': ['Read', 'Edit', 'Bash'], 'cursor': ['Read', 'ApplyPatch', 'Shell'],
        'codex': ['exec_command', 'apply_patch', 'exec_command'], 'opencode': ['read', 'edit', 'shell'],
        'hermes': ['read_file', 'patch', 'terminal'], 'openclaw': ['read', 'edit', 'exec'],
        'claude-desktop': ['read_file', 'edit_block', 'start_process'],
        'workbuddy': ['read_file', 'edit_block', 'start_process'],
    }
    require(names == expected[runtime])
    target = '{workspace}/' + workflow['target']
    if runtime == 'codex':
        require(arguments[0] == {'cmd': "sed -n '1,80p' src/calculator.py", 'workdir': '{workspace}'})
    else:
        require(arguments[0] == {'file_path' if runtime == 'claude-code' else 'path': target})
    before = case['workspace_files'][workflow['target']]
    require(outputs[0] == before)
    ast.parse(before)
    edit = arguments[1]
    if runtime in ('codex', 'cursor'):
        require(isinstance(edit, str))
        lines = edit.splitlines()
        require(lines[:3] == ['*** Begin Patch', '*** Update File: ' + workflow['target'], '@@'])
        require(lines[-1] == '*** End Patch')
        hunk = lines[3:-1]
        require(all(line and line[0] in ' +-' for line in hunk))
        old = ''.join(line[1:] + '\n' for line in hunk if line[0] in ' -')
        new = ''.join(line[1:] + '\n' for line in hunk if line[0] in ' +')
    elif runtime == 'openclaw':
        require(set(edit) == {'path', 'edits'} and edit['path'] == target)
        require(isinstance(edit['edits'], list) and len(edit['edits']) == 1)
        replacement = edit['edits'][0]
        require(set(replacement) == {'oldText', 'newText'})
        old, new = replacement['oldText'], replacement['newText']
    else:
        path_key = 'file_path' if runtime in ('claude-code', 'claude-desktop', 'workbuddy') else 'path'
        old_key, new_key = ('oldString', 'newString') if runtime == 'opencode' else ('old_string', 'new_string')
        require(set(edit) == {path_key, old_key, new_key} and edit[path_key] == target)
        old, new = edit[old_key], edit[new_key]
    require(old and before.count(old) == 1)
    after = before.replace(old, new, 1)
    require(after == workflow['after'])
    ast.parse(after)
    run = arguments[2]
    command_key = 'cmd' if runtime == 'codex' else 'command'
    require(run[command_key] in ('python3 tests/check_calculator.py', 'cd "{workspace}" && python3 tests/check_calculator.py'))
    if runtime in ('claude-desktop', 'workbuddy'):
        require(set(run) == {'command', 'timeout_ms'} and type(run['timeout_ms']) is int and run['timeout_ms'] > 0)
    elif runtime in ('codex', 'opencode', 'openclaw', 'cursor'):
        cwd_key = 'working_directory' if runtime == 'cursor' else 'workdir'
        require(set(run) == {command_key, cwd_key} and run[cwd_key] == '{workspace}')
    elif runtime == 'claude-code':
        require(set(run) == {'command', 'description'} and isinstance(run['description'], str))
    else:
        require(set(run) == {'command'})
    ast.parse(case['workspace_files'][workflow['test']])
    require(outputs[2] == 'calculator checks passed\n')
    if external:
        with tempfile.TemporaryDirectory(prefix='privacy-workflow-replay-') as directory:
            root = Path(directory)
            for name in (workflow['target'], workflow['test']):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(case['workspace_files'][name])
            environment = {'PATH': os.environ.get('PATH', ''), 'HOME': directory, 'PYTHONDONTWRITEBYTECODE': '1'}
            def run_checks():
                return subprocess.run([sys.executable, workflow['test']], cwd=root, env=environment,
                                      capture_output=True, text=True, timeout=10)
            failing = run_checks()
            require(failing.returncode != 0 and 'AssertionError' in failing.stderr)
            (root / workflow['target']).write_text(after)
            passing = run_checks()
            require(passing.returncode == 0 and passing.stdout == outputs[2])


def validate(cases, root=ROOT, external=False):
    checked, deferred = [], []
    for case in cases:
        for spec in case.get('validity', []):
            identity = case['id'] + ':' + spec['format']
            value = at(case['input'], spec['pointer'])
            kind = spec['format']
            try:
                if kind == 'python':
                    ast.parse(value)
                elif kind == 'javascript':
                    if not external:
                        deferred.append(identity)
                        continue
                    command(['node', '--check'], value)
                elif kind == 'shell':
                    if not external:
                        deferred.append(identity)
                        continue
                    command(['sh', '-n'], value)
                elif kind in ('ini', 'aws-ini'):
                    ini = configparser.ConfigParser(interpolation=None)
                    ini.read_string(value)
                    if kind == 'aws-ini':
                        profile = ini['reporting']
                        require(len(profile['aws_access_key_id']) == 20)
                        require(len(profile['aws_secret_access_key']) == 40)
                elif kind == 'json':
                    json.loads(value)
                elif kind == 'json-value':
                    require(json.loads(json.dumps(value, ensure_ascii=False)) == value)
                elif kind in ('http-basic', 'http-jwt'):
                    first, rest = value.split('\r\n', 1)
                    method, path, version = first.split(' ')
                    require(method == 'GET' and path.startswith('/') and version == 'HTTP/1.1')
                    require(value.endswith('\r\n\r\n'))
                    headers = http.client.parse_headers(io.BytesIO(rest.encode('ascii')))
                    require(headers['Host'].endswith('.example.invalid'))
                    scheme, credential = headers['Authorization'].split(' ', 1)
                    if kind == 'http-basic':
                        require(scheme == 'Basic')
                        decoded = base64.b64decode(credential, validate=True).decode('ascii')
                        require(decoded == 'reporting:Maple-Harbor!42')
                    else:
                        require(scheme == 'Bearer')
                        head, body, signature = credential.split('.')
                        decode = lambda text: base64.urlsafe_b64decode(text + '=' * (-len(text) % 4))
                        require(json.loads(decode(head)) == {'alg': 'HS256', 'typ': 'JWT'})
                        claims = json.loads(decode(body))
                        require(claims['iat'] < claims['exp'] and claims['aud'] == 'https://api.example.invalid')
                        expected = hmac.digest(b'privacy-benchmark-only-signing-key', (head + '.' + body).encode(), 'sha256')
                        require(hmac.compare_digest(decode(signature), expected))
                elif kind == 'postgresql-uri':
                    uri = urlsplit(value)
                    require(uri.scheme == 'postgresql' and uri.hostname == 'db.example.invalid' and uri.port == 5432)
                    require(uri.username == 'reporting' and unquote(uri.password) == 'Sparrow,Sunset!42')
                    require(uri.path == '/reports' and uri.query == 'sslmode=require')
                elif kind == 'pem-pair':
                    private = root / 'fixtures/signing-private.pem'
                    public = root / 'fixtures/signing-public.pem'
                    for path, label in ((private, 'PRIVATE KEY'), (public, 'PUBLIC KEY')):
                        pem = path.read_text().strip()
                        require(pem in value)
                        lines = pem.splitlines()
                        require(lines[0] == f'-----BEGIN {label}-----' and lines[-1] == f'-----END {label}-----')
                        require(base64.b64decode(''.join(lines[1:-1]), validate=True).startswith(b'\x30'))
                    if not external:
                        deferred.append(identity)
                        continue
                    command(['openssl', 'pkey', '-in', str(private), '-check', '-noout'])
                    require(command(['openssl', 'pkey', '-in', str(private), '-pubout']).strip() == public.read_text().strip())
                elif kind == 'png-base64':
                    validate_png(value)
                    if spec.get('same_as'):
                        require(value == at(case['input'], spec['same_as']))
                elif kind == 'png-data-url':
                    require(value.startswith('data:image/png;base64,'))
                    encoded = value.split(',', 1)[1]
                    validate_png(encoded)
                    require(encoded == at(case['input'], spec['same_image_as']))
                elif kind == 'markdown-links':
                    destinations = re.findall(r'\[[^\]]+\]\(([^)]+)\)', value)
                    require(destinations == spec['destinations'])
                    require(all(not urlsplit(p).scheme and not p.startswith('/') for p in destinations))
                elif kind == 'reference-urls':
                    urls = re.findall(r'https://\S+', value)
                    require(len(urls) == 2)
                    require(all(urlsplit(url).hostname.endswith('.example.invalid') for url in urls))
                elif kind == 'agent-workflow':
                    validate_workflow(case, external)
                    if not external:
                        deferred.append(identity)
                        continue
                else:
                    raise ValueError('Unknown fixture format: ' + kind)
                checked.append(identity)
            except (AssertionError, ValueError, KeyError, IndexError, SyntaxError, struct.error) as error:
                raise ValueError('Invalid fixture ' + identity) from error
    return {'checked': checked, 'external_validation_required': deferred}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--external', action='store_true', help='Also run syntax/key checks and replay the fixed calculator tests in temporary projects')
    args = parser.parse_args()
    result = validate(json.loads((ROOT / 'cases/synthetic.json').read_text()), external=args.external)
    print(json.dumps(result, indent=2))
