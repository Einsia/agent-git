"""Wire adaptation only: all scanning and publication decisions belong to the CLI."""
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import struct
import subprocess
import time

from sources import BenchError

ADAPTER_VERSION = 1
SESSION = 'agit-' + 'a' * 40
TOKEN = re.compile(r'‹(?:secret|custom):[a-z0-9-]+:[a-z2-7]{12}›|\{\{AGIT_SECRET_V[12]:[^{}]+\}\}')
ACTIVE = set()


def dumps(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'))


def clean_env(root):
    return {'PATH': '/usr/bin:/bin', 'HOME': str(root), 'AGIT_HOME': str(root / 'agit'),
            'XDG_CONFIG_HOME': str(root / 'config'), 'XDG_CACHE_HOME': str(root / 'cache'),
            'TMPDIR': str(root), 'AGIT_USE_SYSTEM_GIT': '1',
            'AGIT_HUB_URL': 'http://127.0.0.1:9', 'AGIT_TELEMETRY_DISABLED': '1',
            'AGIT_INTERNAL_UPDATE_RESTART': '1',
            'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
            'GIT_TERMINAL_PROMPT': '0', 'RAYON_NUM_THREADS': '1', 'LC_ALL': 'C.UTF-8'}


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()
    ACTIVE.discard(process)
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            stream.close()


def cleanup():
    for process in list(ACTIVE):
        terminate(process)


def version(binary, root):
    root.mkdir(parents=True, exist_ok=True)
    process = subprocess.Popen([str(binary), '--version'], cwd=root, env=clean_env(root),
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    ACTIVE.add(process)
    try:
        output, _ = process.communicate(timeout=20)
        if process.returncode != 0 or len(output) > 512:
            raise BenchError('CLI version verification failed')
        return output.decode().strip().removeprefix('agit ')
    finally:
        terminate(process)


class Worker:
    def __init__(self, binary, root, entropy=False, policy=None):
        self.root = root
        self.root.mkdir(parents=True)
        self.env = clean_env(root)
        self.repo = None
        (root / 'agit').mkdir()
        (root / 'agit/config.json').write_text(dumps({'privacy.entropy': 'replace' if entropy else 'report'}))
        if policy is not None:
            self.repo = root / 'workspace'
            self.repo.mkdir()
            subprocess.run(['git', 'init', '-q', str(self.repo)], env=self.env,
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            (self.repo / '.git/agit').mkdir()
            (self.repo / 'src').mkdir()
            config = json.loads(dumps(policy).replace('{workspace}', str(self.repo)))
            config['workspace'] = str(self.repo)
            (self.repo / '.git/agit/privacy-policy.json').write_text(dumps(config))
        self.process = subprocess.Popen([str(binary), '__privacy-worker-v1'], cwd=root,
                                        env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, start_new_session=True, bufsize=0)
        ACTIVE.add(self.process)
        self.serial = 0
        self.raw = []

    def close(self):
        terminate(self.process)

    def read(self, length, deadline):
        data = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while len(data) < length:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise BenchError('CLI request timed out')
                part = os.read(self.process.stdout.fileno(), length - len(data))
                if not part:
                    raise BenchError('CLI worker closed its protocol stream')
                data.extend(part)
        return data

    def request(self, mode, text, context=''):
        self.serial += 1
        request = {'id': self.serial, 'deadline_ms': 15000, 'repo': str(self.repo) if self.repo else None,
                   'mode': mode, 'text': text, 'context': context}
        payload = dumps(request).encode()
        deadline = time.monotonic() + 20
        self.process.stdin.write(struct.pack('>I', len(payload)) + payload)
        size = struct.unpack('>I', self.read(4, deadline))[0]
        if size > 16 * 1024 * 1024:
            raise BenchError('CLI response exceeds the protocol budget')
        response = json.loads(self.read(size, deadline))
        self.raw.append({'mode': mode, 'response': response})
        if response.get('id') != self.serial or not isinstance(response.get('content'), str):
            raise BenchError('CLI response does not match the request')
        return response

    def manage(self, action, secret=None, identity=None):
        response = self.request('manage', dumps({'action': action, 'global': True, 'id': identity,
                                                'name': 'synthetic benchmark rule', 'secret': secret}))
        if response['status'] != 'complete':
            raise BenchError('CLI could not load the synthetic rule configuration')
        return json.loads(response['content'])

    def rules(self, case):
        for value in case.get('allows', []):
            identity = self.manage('identify', dumps([value]))[0]
            self.manage('allow', identity=identity)
        for value in case.get('blocks', []):
            self.manage('add', value)

    def prepare(self, case):
        response = self.request('prepare', '')
        if response['status'] != 'complete':
            raise BenchError('CLI could not initialize the isolated privacy worker')
        self.rules(case)

    def run(self, case, prepared=False):
        if not prepared:
            self.prepare(case)
        value = case['input']
        if self.repo:
            value = json.loads(dumps(value).replace('{workspace}', str(self.repo)))
        if case['layer'] == 'policy':
            log = ''.join(json.dumps({'_source': 'claude-code', '_session_id': SESSION,
                                 '_object_hash': hashlib.sha256(dumps(record).encode()).hexdigest()[:40],
                                 'content': json.loads(dumps(record))}, ensure_ascii=False, separators=(',', ':')) + '\n' for record in value['records'])
            metadata = {'session': SESSION, 'runtime': 'claude-code',
                        'cwd': str(self.repo) if self.repo else '/bench/workspace', **value.get('metadata', {})}
            if case.get('mode') == 'plain':
                response = self.request('protect_envelopes', log)
                if response['status'] != 'complete':
                    return {'status': response['status'], 'reason': response.get('reason')}
                records = [json.loads(line)['content'] for line in response['content'].splitlines()]
                return {'status': 'complete', 'output': {'records': records, 'metadata': metadata},
                        'contracts': {'records_preserved': len(records) == len(value['records'])}}
            response = self.request('project_publication', dumps({'branch': None, 'log': log, 'view': log,
                                                                 'metadata': metadata, 'additional': []}))
            if response['status'] != 'complete':
                return {'status': response['status'], 'reason': response.get('reason')}
            projected = json.loads(response['content'])
            output = {'records': [json.loads(line)['content'] for line in projected['log'].splitlines()],
                      'metadata': projected['metadata']}
            shape_valid = all(
                isinstance(record, dict) and record.get('type') in ('user', 'assistant')
                and isinstance(record.get('message'), dict)
                and record['message'].get('role') == record['type']
                and isinstance(record['message'].get('content'), list)
                and bool(record['message']['content']) for record in output['records'])
            expected_calls = sum(b.get('type') == 'tool_use' for record in value['records']
                                 for b in (record.get('message', {}).get('content', [])
                                           if isinstance(record.get('message', {}).get('content'), list) else []))
            expected_results = sum(b.get('type') == 'tool_result' for record in value['records']
                                   for b in (record.get('message', {}).get('content', [])
                                             if isinstance(record.get('message', {}).get('content'), list) else []))
            calls, results = [], []
            for record in output['records']:
                blocks = record.get('message', {}).get('content', [])
                if isinstance(blocks, list):
                    calls.extend(b['id'] for b in blocks if b.get('type') == 'tool_use' and 'id' in b)
                    results.extend(b['tool_use_id'] for b in blocks if b.get('type') == 'tool_result' and 'tool_use_id' in b)
            return {'status': 'complete', 'output': output, 'policy_report': projected['report'],
                    'contracts': {'public_record_shape': shape_valid,
                                  'paired_calls': len(calls) == len(set(calls)) == expected_calls
                                  and len(results) == expected_results and all(i in calls for i in results),
                                  'view_matches_log': projected['view'] == projected['log'],
                                  'records_preserved': len(output['records']) == len(value['records'])}}
        mode = 'protect_text' if isinstance(value, str) else 'protect_jsonl'
        text = value if isinstance(value, str) else dumps(value) + '\n'
        if case.get('stream_cuts'):
            pending, context, protected_parts, offset = '', '', [], 0
            for cut in [*case['stream_cuts'], len(text)]:
                pending += text[offset:cut]
                offset = cut
                response = self.request('protect_stream', pending, context)
                if response['status'] != 'complete' or response.get('reason'):
                    return {'status': 'partial', 'reason': response.get('reason')}
                consumed = response.get('consumed')
                raw = pending.encode()
                if not isinstance(consumed, int) or not 0 <= consumed <= len(raw):
                    raise BenchError('Invalid stream consumption coordinate')
                context += raw[:consumed].decode()
                pending = raw[consumed:].decode()
                protected_parts.append(response['content'])
            response = self.request('protect_text', pending, context)
            response = {**response, 'content': ''.join(protected_parts) + response['content']}
        else:
            response = self.request(mode, text, case.get('context', ''))
        if response['status'] != 'complete' or response.get('reason'):
            return {'status': 'partial' if response['status'] == 'complete' else response['status'],
                    'reason': response.get('reason'), 'output': response['content']}
        protected = response['content']
        restored = self.request('hydrate_text' if isinstance(value, str) else 'hydrate_jsonl', protected)
        repeated = self.request(mode, protected)
        output = protected if isinstance(value, str) else json.loads(protected)
        restored_value = restored['content'] if isinstance(value, str) else json.loads(restored['content'])
        repeated_value = repeated['content'] if isinstance(value, str) else json.loads(repeated['content'])
        mappings = {}
        for token in set(TOKEN.findall(protected)):
            hydrated = self.request('hydrate_text', token)
            if hydrated['status'] != 'complete' or hydrated['content'] == token:
                raise BenchError('Cannot recover a synthetic placeholder for range alignment')
            mappings[token] = hydrated['content']
        return {'status': 'complete', 'output': output, 'mappings': mappings,
                'detection': None,
                'range_source': 'verified dictionary recovery, not detector telemetry',
                'contracts': {'restoration': restored['status'] == 'complete' and restored_value == value,
                              'idempotence': repeated['status'] == 'complete' and repeated_value == output}}


def telemetry(binary, root, request):
    root.mkdir(parents=True, exist_ok=True)
    process = subprocess.Popen([str(binary), '__privacy-bench-v1'], cwd=root, env=clean_env(root),
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                               start_new_session=True)
    ACTIVE.add(process)
    try:
        out, _ = process.communicate(dumps({'schema_version': 1, **request}).encode(), timeout=20)
        if len(out) > 4 * 1024 * 1024:
            raise BenchError('Detector telemetry exceeds its output budget')
        if process.returncode:
            return None
        value = json.loads(out)
        if value.get('schema_version') != 1:
            raise BenchError('Unsupported detector telemetry schema')
        return value
    finally:
        terminate(process)
