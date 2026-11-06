"""Synthetic workflows with expectations chosen from user intent and public format contracts."""
import base64
import hmac
import json
import re
import struct
import zlib
from urllib.parse import quote

POLICY_BASIS = 'docs/privacy-plain-publication.md; docs/secret-context-classification.md'
BASIC_BASIS = 'https://www.rfc-editor.org/rfc/rfc7617'
JWT_BASIS = 'https://www.rfc-editor.org/rfc/rfc7519'
GITLAB_BASIS = 'https://docs.gitlab.com/security/tokens/'
AWS_BASIS = 'https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-files.html'
JWT_KEY = b'privacy-benchmark-only-signing-key'
GH_TOKEN = 'ghp_Q8wE4rT7yU2iO9pA5sD3fG6hJ1kL0zXcVbNm'
GL_TOKEN = 'glpat-R8qW3eT6yU2iP9aS5dF7'
PASSPHRASE = 'maple-river-7'
OPAQUE = 'u7JmQ3pV9cL2xR8hF4aN6wK1sD5bG0tY'
PLACEHOLDER = r'(?:‹secret:[a-z0-9-]+:[a-z2-7]{12}›|\{\{AGIT_SECRET_V[12]:[^{}]+\}\})'


def jwt_token():
    def encode(value):
        return base64.urlsafe_b64encode(json.dumps(value, separators=(',', ':'), sort_keys=True).encode()).rstrip(b'=')
    head = encode({'alg': 'HS256', 'typ': 'JWT'})
    payload = encode({'sub': 'bench-service', 'aud': 'https://api.example.invalid', 'iat': 1793923200, 'exp': 4102444800})
    message = head + b'.' + payload
    signature = base64.urlsafe_b64encode(hmac.digest(JWT_KEY, message, 'sha256')).rstrip(b'=')
    return (message + b'.' + signature).decode()


def png_icon():
    def chunk(kind, body):
        return struct.pack('>I', len(body)) + kind + body + struct.pack('>I', zlib.crc32(kind + body))
    rows = (b'\x00' + b'\x28\x78\xd0\xff' * 16) * 16
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 16, 16, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))


def protected_text_pattern(text, secret):
    return PLACEHOLDER.join(re.escape(part) for part in text.split(secret))


def add_scenarios(secret, policy, check, message, binding, root):
    def text_case(name, value, protected, scenario, expectation, *, basis=POLICY_BASIS, validity=(), **options):
        secret('scenario-' + name, value, protected if isinstance(protected, dict) else {'': protected}, expectation,
               scenario=scenario, expectation_basis=basis, validity=list(validity),
               source='Constructed from a realistic workflow with fixed synthetic values; expected behavior is independent of CLI output.',
               **options)

    python_settings = f'''import os
from dataclasses import dataclass

@dataclass
class Settings:
    password: str
    api_key: str

settings = Settings(password=os.environ["DB_PASSWORD"], api_key=os.environ["API_KEY"])
connection = {{"host": "db.example.invalid", "password": "{PASSPHRASE}", "timeout": 30}}
'''
    text_case('python-settings-review', python_settings, [PASSPHRASE],
              'A user pastes a complete Python settings module while reviewing an accidentally hardcoded database password.',
              'Protect the password literal; retain annotations, environment lookups, host and timeout.',
              validity=[{'format': 'python', 'pointer': ''}])
    provider_code = '''class OAuthProviderV2:
    def __init__(self, client_id):
        self.client_id = client_id

token = OAuthProviderV2("desktop-app")
password = OAuthProviderV2
'''
    text_case('python-provider-references', provider_code, [],
              'A user asks about a Python factory and variables that refer to a provider class, rather than stored credentials.',
              'Preserve the valid class, constructor call and class reference, including the version digit in its name.',
              validity=[{'format': 'python', 'pointer': ''}])
    javascript = f'''const apiKey = process.env.API_KEY;
const options = {{ timeout: 5000, password: "{PASSPHRASE}" }};
function clientOptions() {{ return {{ apiKey, ...options }}; }}
module.exports = clientOptions;
'''
    text_case('javascript-client-options', javascript, [PASSPHRASE],
              'A Node.js client configuration is pasted into an assistant chat to debug authentication.',
              'Protect the literal password without changing the environment reference, object syntax or timeout.',
              validity=[{'format': 'javascript', 'pointer': ''}])
    ini = f'[database]\nhost = db.example.invalid\nuser = reporting\npassword = {PASSPHRASE}\nport = 5432\n'
    text_case('ini-database-passphrase', ini, [PASSPHRASE],
              'An application operator pastes its INI database section while troubleshooting a connection.',
              'Protect a human-readable passphrase even though it is less random than an API key; keep the remaining INI fields.',
              validity=[{'format': 'ini', 'pointer': ''}])
    dotenv = 'readonly password="OAuthProviderV2"\nexport password\nprintf "%s\\n" "Configuration loaded"\n'
    text_case('shell-password-value', dotenv, ['OAuthProviderV2'],
              'A startup script sets an actual password to a word also used as a class name in source code.',
              'Protect the assigned password in this executable shell script; spelling alone does not make it a source reference.',
              validity=[{'format': 'shell', 'pointer': ''}])
    curl_reference = '''#!/bin/sh
set -eu
curl --fail --header "Authorization: Bearer ${ACCESS_TOKEN}" \\
  --user "${DB_USER}:${DB_PASSWORD}" https://api.example.invalid/status
'''
    text_case('curl-environment-references', curl_reference, [],
              'A README provides a runnable shell command that obtains credentials from the caller environment.',
              'Keep variable references and the command intact; no credential value is present in the pasted script.',
              validity=[{'format': 'shell', 'pointer': ''}])
    oauth = {'session': {'access_token': OPAQUE, 'token_type': 'Bearer', 'expires_in': 3600},
             'page': {'nextPageToken': OPAQUE, 'items': [], 'has_more': True}}
    text_case('oauth-and-pagination', oauth, {'/session/access_token': [OPAQUE]},
              'A gateway debug response includes an authentication session and a pagination cursor; both happen to use the same opaque string.',
              'Protect the access token and retain the pagination cursor, empty collection, boolean and expiry.',
              validity=[{'format': 'json-value', 'pointer': ''}])
    basic = base64.b64encode(b'reporting:Maple-Harbor!42').decode()
    request = f'GET /reports HTTP/1.1\r\nHost: reports.example.invalid\r\nAuthorization: Basic {basic}\r\nAccept: application/json\r\n\r\n'
    text_case('http-basic-authentication', request, [basic],
              'An HTTP diagnostic trace exposes a Basic Authorization header for a reporting endpoint.',
              'Protect the complete encoded username/password credential, preserving the scheme and other request fields.',
              basis=BASIC_BASIS, validity=[{'format': 'http-basic', 'pointer': ''}])
    jwt = jwt_token()
    request = f'GET /v1/jobs HTTP/1.1\r\nHost: api.example.invalid\r\nAuthorization: Bearer {jwt}\r\nX-Request-ID: {OPAQUE}\r\n\r\n'
    text_case('http-signed-jwt', request, [jwt],
              'A developer shares a gateway request trace containing a signed service JWT and an unrelated request identifier.',
              'Protect the complete bearer token, including its signature; preserve the request identifier and HTTP syntax.',
              basis=JWT_BASIS, validity=[{'format': 'http-jwt', 'pointer': ''}])
    encoded_password = quote('Sparrow,Sunset!42', safe='')
    uri = f'postgresql://reporting:{encoded_password}@db.example.invalid:5432/reports?sslmode=require'
    text_case('postgresql-encoded-password', uri, [encoded_password],
              'A database connection URI is copied from a migration error into the conversation.',
              'Protect the percent-encoded password while retaining the username, host, port, database and query.',
              basis='https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNSTRING-URIS',
              validity=[{'format': 'postgresql-uri', 'pointer': ''}])
    gitlab = f'curl --fail --header "PRIVATE-TOKEN: {GL_TOKEN}" https://gitlab.example.invalid/api/v4/projects\n'
    text_case('gitlab-api-command', gitlab, [GL_TOKEN],
              'A developer pastes the curl command used to diagnose a GitLab API permission failure.',
              'Protect the synthetic GitLab personal token; retain the header name, endpoint and command structure.',
              basis=GITLAB_BASIS, validity=[{'format': 'shell', 'pointer': ''}])
    aws_id, aws_secret = 'AKIAX7Y6Z5W4V3U2T1S0', 'vR7pL2xQ9mN4aS8dF1gH6jK3cW5yU0iOeTbZsAqC'
    aws = f'[reporting]\naws_access_key_id = {aws_id}\naws_secret_access_key = {aws_secret}\n'
    text_case('aws-shared-credentials', aws, [aws_secret],
              'A user shares a named AWS credentials profile after selecting the wrong account for a deployment.',
              'Protect the secret access key; retain the profile name, non-secret access key identifier and INI layout.',
              basis=AWS_BASIS + '; https://docs.aws.amazon.com/us_en/IAM/latest/UserGuide/troubleshoot.html',
              validity=[{'format': 'aws-ini', 'pointer': ''}])
    private = (root / 'fixtures/signing-private.pem').read_text().strip()
    public = (root / 'fixtures/signing-public.pem').read_text().strip()
    text_case('signing-key-pair', 'Signing public key:\n' + public + '\nSigning private key:\n' + private + '\n', [private],
              'A service operator pastes a public/private signing-key pair to diagnose a key-format mismatch.',
              'Protect the entire valid private PEM block, including its delimiters; retain the matching public key.',
              basis='https://www.rfc-editor.org/rfc/rfc7468', validity=[{'format': 'pem-pair', 'pointer': ''}])
    escaped_password = 'Slate"Lake\\Trail42!'
    escaped = {'database': {'password': escaped_password, 'host': 'db.example.invalid'},
               'message': 'The server said "authentication failed".\nRetry after rotation.', 'attempt': 2, 'retry': False}
    text_case('json-escaped-password', escaped, {'/database/password': [escaped_password]},
              'A JSON diagnostics payload contains a password with a quote and a backslash, plus an escaped error message.',
              'Protect the complete decoded password while retaining escapes, neighboring text and scalar types.',
              validity=[{'format': 'json-value', 'pointer': ''}])
    # This is a Unicode-coordinate fixture; its non-English diagnostic text is test data.
    diagnostic = f'连接失败 🔐: password="{PASSPHRASE}"\nCafé service: retry in 30 seconds.\n'
    text_case('unicode-authentication-log', diagnostic, [PASSPHRASE],
              'A localized authentication log is pasted with CJK text, an emoji and an accented service label.',
              'Protect the password at the correct character and byte coordinates without damaging surrounding Unicode.')
    eof = f'export DB_PASSWORD="{PASSPHRASE}"'
    text_case('stream-password-at-eof', eof, [PASSPHRASE],
              'A terminal result arrives incrementally and ends directly after a quoted password assignment, without a newline.',
              'Protect a credential split across chunks and the final flush; retain the valid shell assignment.',
              validity=[{'format': 'shell', 'pointer': ''}], stream_cuts=[eof.index(PASSPHRASE) + 3])
    stream = f'export DB_PASSWORD="{PASSPHRASE}"\nexport GITLAB_TOKEN="{GL_TOKEN}"\nprintf "%s\\n" "Ready"\n'
    text_case('stream-multiple-credentials', stream, [PASSPHRASE, GL_TOKEN],
              'An assistant receives a startup script in chunks that split both a password and a provider token.',
              'Protect every occurrence across successive chunks while preserving complete public lines and recovery.',
              validity=[{'format': 'shell', 'pointer': ''}],
              stream_cuts=[stream.index(PASSPHRASE) + 4, stream.index(GL_TOKEN) + 8, len(stream) - 5])
    blocked = {'current_deployment': 'north-harbor-2026', 'previous_deployment': 'north-harbor', 'status': 'ready'}
    text_case('overlapping-user-secrets', blocked,
              {'/current_deployment': ['north-harbor-2026'], '/previous_deployment': ['north-harbor']},
              'A team explicitly protects both its current deployment code and the shorter code used by the previous deployment.',
              'Protect the complete longer value and the separate shorter value; an overlapping block must not expose a suffix.',
              blocks=['north-harbor', 'north-harbor-2026'], validity=[{'format': 'json-value', 'pointer': ''}])

    def conversation(prompt, calls, results, cwd=None):
        first = message(prompt)
        if cwd:
            first['cwd'] = cwd
        return [first, {'type': 'assistant', 'message': {'role': 'assistant', 'content': calls}},
                {'type': 'user', 'message': {'role': 'user', 'content': results}}]

    def call(identity, path=None, name='Read', arguments=None):
        return {'type': 'tool_use', 'id': identity, 'name': name,
                'input': arguments if arguments is not None else {'file_path': path}}

    def result(identity, text):
        return {'type': 'tool_result', 'tool_use_id': identity, 'content': text}

    def policy_case(name, records, checks, scenario, expectation, *, config=None, validity=(), **options):
        policy('scenario-' + name, records, checks, expectation, config=config, scenario=scenario,
               expectation_basis=POLICY_BASIS, validity=list(validity),
               source='Synthetic task transcript with native tool schemas and independently reviewed content expectations.', **options)

    public_body = f'API_TOKEN = "{GH_TOKEN}"\nTIMEOUT_SECONDS = 30\nFEATURE_ENABLED = True\n'
    private_body = 'LOG_LEVEL=debug\nPORT=8080\n'
    records = conversation('Compare the local environment file with the checked-in service configuration.',
        [call('read-env', '{workspace}/.env'), call('read-code', '{workspace}/src/service_config.py')],
        [result('read-code', public_body), result('read-env', private_body)])
    public_result, private_result = '/records/2/message/content/0/content', '/records/2/message/content/1/content'
    policy_case('files-and-secret-protection', records,
        [check('/records/0/message/content/0/text', 'keep', records[0]['message']['content'], input_pointer='/records/0/message/content'),
         check('/records/1/message/content/0/name', 'rewrite', 'Read', 'omitted'),
         check('/records/1/message/content/0/input', 'rewrite', {'file_path': '{workspace}/.env'}, {'notice': '[File content excluded by policy]'}),
         check('/records/1/message/content/1/name', 'keep', 'Read'),
         check('/records/1/message/content/1/input', 'rewrite', {'file_path': '{workspace}/src/service_config.py'}, {'file_path': '<workspace>/src/service_config.py'}),
         check(private_result, 'hide', private_body, '[File content excluded by policy]'),
         check(public_result, 'rewrite', public_body, replacement_pattern=protected_text_pattern(public_body, GH_TOKEN))],
        'The assistant reads a sensitive environment file and a public Python file containing an accidentally committed token; results return in reverse call order.',
        'Hide the environment result even though it has no credential; retain the Python file with only its token protected and preserve call associations.',
        workspace_files={'.env': private_body, 'src/service_config.py': public_body},
        validity=[{'format': 'python', 'pointer': public_result}, {'format': 'shell', 'pointer': private_result}])
    ordinary = '{"log_level":"info","retries":3}\n'
    records = conversation('Inspect the local settings from the source directory.',
        [call('read-relative-env', '../.env'), call('read-relative-config', './config.json')],
        [result('read-relative-env', private_body), result('read-relative-config', ordinary)], cwd='{workspace}/src')
    policy_case('relative-paths-and-cwd', records,
        [check('/records/1/message/content/0/input', 'rewrite', {'file_path': '../.env'}, {'notice': '[File content excluded by policy]'}),
         check('/records/2/message/content/0/content', 'hide', private_body, '[File content excluded by policy]'),
         check('/records/1/message/content/1/input', 'rewrite', {'file_path': './config.json'}, {'file_path': '<workspace>/src/config.json'}),
         check('/records/2/message/content/1/content', 'keep', ordinary)],
        'A conversation runs in the source subdirectory and reads a parent environment file and a sibling JSON configuration.',
        'Resolve relative paths from the captured working directory; exclude the environment file and retain the ordinary configuration.',
        workspace_files={'.env': private_body, 'src/config.json': ordinary},
        validity=[{'format': 'json', 'pointer': '/records/2/message/content/1/content'}])
    records = conversation('Check the local configuration and the public deployment documentation.',
        [call('local-read', '{workspace}/src/config.json'), call('docs-search', name='mcp__docs__search', arguments={'query': 'deployment configuration'})],
        [result('local-read', ordinary), result('docs-search', 'Set the deployment region before starting the worker.')])
    policy_case('tool-omit-with-docs-exception', records,
        [check('/records/1/message/content/0/name', 'rewrite', 'Read', 'omitted'),
         check('/records/1/message/content/0/input', 'rewrite', {'file_path': '{workspace}/src/config.json'}, {'notice': '[Tool call omitted by policy]'}),
         check('/records/2/message/content/0/content', 'hide', ordinary, '[Tool output omitted by policy]'),
         check('/records/1/message/content/1/name', 'keep', 'mcp__docs__search'),
         check('/records/1/message/content/1/input', 'keep', {'query': 'deployment configuration'}),
         check('/records/2/message/content/1/content', 'keep', 'Set the deployment region before starting the worker.')],
        'A repository omits local tools from public views while explicitly allowing a documentation-search MCP tool.',
        'Apply the tool exception to both the allowed call and result, without exposing the omitted local result.',
        config={'tools': {'omit': True, 'overrides': {'mcp__docs__search': False}}}, workspace_files={'src/config.json': ordinary})
    caption = f'The icon upload returns 401. My request uses Authorization: Bearer {OPAQUE}. Please check the header.'
    encoded_icon = base64.b64encode(png_icon()).decode()
    records = [{'type': 'user', 'message': {'role': 'user', 'content': [
        {'type': 'text', 'text': caption},
        {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': encoded_icon}}]}}]
    policy_case('image-metadata-and-caption-secret', records,
        [check('/records/0/message/content/0/text', 'rewrite', caption, replacement_pattern=protected_text_pattern(caption, OPAQUE)),
         check('/records/0/message/content/1', 'hide', encoded_icon,
               {'type': 'text', 'text': '[Attachment body omitted; metadata retained: {"media_type":"image/png","type":"image"}]'},
               input_pointer='/records/0/message/content/1/source/data')],
        'An icon-upload bug report includes a valid PNG and a pasted bearer header in its caption.',
        'Retain attachment metadata and the useful caption, omit image bytes, and protect the caption credential.',
        config={'attachments': 'metadata'}, validity=[{'format': 'png-base64', 'pointer': '/records/0/message/content/1/source/data'}])
    arguments = json.dumps({'cmd': 'printenv DEPLOY_TOKEN', 'workdir': '{workspace}'}, sort_keys=True)
    records = [
        {'type': 'response_item', 'payload': {'type': 'function_call', 'name': 'exec_command', 'call_id': 'call_deploy_check', 'arguments': arguments}},
        {'type': 'response_item', 'payload': {'type': 'function_call_output', 'call_id': 'call_deploy_check', 'output': GH_TOKEN + '\n'}},
    ]
    layout = [
        {'role': 'assistant', 'preserve': ['/records/0/type', '/records/0/payload/type'], 'blocks': [
            {'type': 'tool_use', 'fields': {'id': binding('/records/0/payload/call_id'), 'name': binding('/records/0/payload/name'),
             'input': binding('/records/0/payload/arguments', True)}}]},
        {'role': 'user', 'preserve': ['/records/1/type', '/records/1/payload/type'], 'blocks': [
            {'type': 'tool_result', 'fields': {'tool_use_id': binding('/records/1/payload/call_id'), 'content': binding('/records/1/payload/output')}}]},
    ]
    policy_case('codex-terminal-secret', records,
        [check('/records/0/message/content/0/name', 'keep', 'exec_command', input_pointer='/records/0/payload/name'),
         check('/records/0/message/content/0/input/cmd', 'rewrite', arguments,
               'printenv DEPLOY_TOKEN', input_pointer='/records/0/payload/arguments'),
         check('/records/0/message/content/0/input/workdir', 'rewrite', arguments,
               replacement_pattern=r'<(?:workspace|private-file-[1-9][0-9]*)>', input_pointer='/records/0/payload/arguments'),
         check('/records/1/message/content/0/content', 'rewrite', GH_TOKEN + '\n',
               replacement_pattern=protected_text_pattern(GH_TOKEN + '\n', GH_TOKEN), input_pointer='/records/1/payload/output')],
        'A Codex terminal call prints an environment variable while diagnosing a deployment authentication failure.',
        'Keep the command and tool association, replace the local workdir with a documented public alias, and protect the printed credential; alias allocation is not a credential decision.',
        runtime='codex', record_layout=layout, validity=[{'format': 'json', 'pointer': '/records/0/payload/arguments'}])
    notes = 'Deployment window: Tuesday.\nOwner: Platform team.\n'
    for branch, hidden in [('release', True), ('feature/docs-review', False)]:
        records = conversation('Read the internal release checklist.',
            [call('read-checklist', '{workspace}/src/internal/checklist.md')], [result('read-checklist', notes)])
        checks = ([check('/records/1/message/content/0/input', 'rewrite', {'file_path': '{workspace}/src/internal/checklist.md'},
                         {'notice': '[File content excluded by policy]'}),
                   check('/records/2/message/content/0/content', 'hide', notes, '[File content excluded by policy]')]
                  if hidden else
                  [check('/records/1/message/content/0/input', 'rewrite', {'file_path': '{workspace}/src/internal/checklist.md'},
                         {'file_path': '<workspace>/src/internal/checklist.md'}),
                   check('/records/2/message/content/0/content', 'keep', notes)])
        policy_case('branch-checklist-' + ('release' if hidden else 'feature'), records, checks,
            'The same internal checklist is captured on a release branch and a documentation-review branch.',
            'Apply the additional release-branch exclusion only on release; preserve the same file on the feature branch.',
            config={'branches': {'release': {'exclude': ['src/internal/**']}}}, branch=branch,
            workspace_files={'src/internal/checklist.md': notes})
