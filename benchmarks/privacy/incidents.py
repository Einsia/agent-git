"""Reconstruct reported failures with synthetic content and explicit evidence boundaries."""
import base64
import hashlib
import json
import re
import struct
import zlib

ISSUES = 'https://git.xiaoaojianghu.fun:114/dev/agentgit/agent-git/-/work_items/'
HUB = 'https://agent-git.com'
TOKEN = 'ghp_Q8wE4rT7yU2iO9pA5sD3fG6hJ1kL0zXcVbNm'
PLACEHOLDER = r'(?:‹secret:[a-z0-9-]+:[a-z2-7]{12}›|\{\{AGIT_SECRET_V[12]:[^{}]+\}\})'
SESSIONS = {
    'import': ('leopierce/night-owl-index', 'agit-f7d113cee8eb65f2e207e8365799680fce623122', '95f9834d4d00a0818957397562a34cddf68900bb'),
    'links': ('jonahreid/weather-poetry', 'agit-a2ee913d7202709cd1acb9803e580992d34b4a7d', '942d3a2713f4f5efa511ec3f98472da2e96f4aae'),
    'ppt': ('einsia/ppt-bench', 'agit-f2ef05924581003865ddaeea71ccbb1c8a53e55b', '11b3295de8de76d70752129783626f02a3a7ca87'),
    'coffee': ('ethanm/coffee-evidence-map', 'agit-758354fc4907620c6ef496cf60810072cac22d64', 'e173cc9aeb00746ecc558f8d4e957b3e0f45a25c'),
    'foreign': ('einsia/AgentGit', 'agit-f554246de2aaa6b938427df2272f31c06e73267a', '618efbbb2a60d67b075d24aa1ac402a08e5b9e4e'),
}


def evidence(issue_ids, session=None, observation='The issue reports the failure; this case reconstructs its local protection boundary.'):
    result = {'issues': [ISSUES + str(i) for i in issue_ids], 'observed_at': '2026-11-06', 'observation': observation}
    if session:
        repo, identity, commit = SESSIONS[session]
        result['session'] = {'url': f'{HUB}/@{repo}/s/{identity}?ref={commit}', 'commit': commit}
    return result


def texture_png():
    def chunk(kind, body):
        return struct.pack('>I', len(body)) + kind + body + struct.pack('>I', zlib.crc32(kind + body))
    rows = b''.join(b'\x00' + b''.join(hashlib.sha256(f'fixture-pixel-{x}-{y}'.encode()).digest()[:3] + b'\xff'
                                     for x in range(32)) for y in range(32))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 32, 32, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))


def protected_pattern(text, value):
    return PLACEHOLDER.join(re.escape(piece) for piece in text.split(value))


def add_incidents(secret, policy, check, message, binding):
    def add_text(name, value, protected, scenario, expectation, provenance, *, validity=(), **options):
        secret('incident-' + name, value, protected if isinstance(protected, dict) else {'': protected}, expectation,
               scenario=scenario, coverage_kind='incident-reconstruction', evidence=provenance,
               expectation_basis='docs/privacy-plain-publication.md; docs/secret-context-classification.md',
               source='Synthetic reconstruction of a reported boundary; no original transcript, credential, dictionary or media bytes are imported.',
               validity=list(validity), **options)

    source = f'''from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

class PreviewHandler(SimpleHTTPRequestHandler):
    pass

server_type = ThreadingHTTPServer
deployment_key = "{TOKEN}"
'''
    add_text('stdlib-import', source, [TOKEN],
             'A local preview server is saved alongside a deployment credential while entropy replacement is enabled.',
             'Retain the standard-library import, class inheritance and source references; protect the adjacent provider credential.',
             evidence([66], 'import', 'The frozen Hub trace contains SimpleHTTPRequestHandler in an import and a Handler subclass. Issue 66 establishes the separate file-commit corruption; the current view is workflow corroboration.'),
             entropy=True, validity=[{'format': 'python', 'pointer': ''}])
    markdown = f'# Deliverables\n\n- [Foldout](artifacts/guide-A4-foldout-2026.pdf)\n- [Reading booklet](output/pdf/seasonal-reading-2026.pdf)\n\nDeployment credential: {TOKEN}\n'
    add_text('relative-artifact-links', markdown, [TOKEN],
             'An agent writes a deliverable index whose relative PDF links must remain usable by another reader.',
             'Keep both Markdown destinations and labels while protecting the adjacent credential. This checks text classification, not file-commit or link-resolution integration.',
             evidence([67], 'links', 'The frozen Hub trace contains the reported relative PDF deliverable path and README handoff. The issue documents corruption during a later local file commit, not successful publication of a broken README.'),
             entropy=True, validity=[{'format': 'markdown-links', 'pointer': '',
                                     'destinations': ['artifacts/guide-A4-foldout-2026.pdf', 'output/pdf/seasonal-reading-2026.pdf']}])
    enums = f'''import zipfile
from pptx.enum.text import MSO_ANCHOR
from pptx.enum.shapes import MSO_SHAPE
from pptx.enum.dml import MSO_LINE_DASH_STYLE

def style_card(shape):
    shape.text_frame.vertical_anchor = MSO_ANCHOR.MIDDLE
    shape.line.dash_style = MSO_LINE_DASH_STYLE.DASH
    return MSO_SHAPE.ROUNDED_RECTANGLE

compression = zipfile.ZIP_DEFLATED
deployment_key = "{TOKEN}"
'''
    add_text('ppt-enum-references', enums, [TOKEN],
             'An agent stores a Python presentation generator using dotted enum constants and a ZIP compression constant.',
             'Preserve executable enum references and module names under entropy opt-in; protect the credential literal independently.',
             evidence([69, 66], 'ppt', 'The frozen presentation session contains MSO_ANCHOR.MIDDLE and MSO_SHAPE references in generated Python. The bulk false-positive counts come from issue 69, not a count inferred from this rendered view.'),
             entropy=True, validity=[{'format': 'python', 'pointer': ''}])
    key = 'sk-proj-' + 'B7mQ2vN9xR4pL8tW1yH6cF3sD5aK0jE7uG2zV9bP4nM8rT6q'
    prompt = f'The staging API rejected this rotated project key: {key}. Please help me check the configuration.'
    add_text('project-key-without-marker', prompt, [key],
             'A user pastes a provider-shaped project key while debugging a rejected request; the body has no embedded vendor marker.',
             'Protect the complete credential-shaped value by default and retain the question and punctuation. A rejected or synthetic key still must not teach the detector to expose that shape.',
             evidence([114, 136], observation='These reports explicitly used generated fake keys. The replacement is also synthetic, not asserted to be an issued or working provider key.'))
    diagnostics = {'headers': {'anthropic-beta': 'interleaved-thinking-2025-05-14,oauth-2025-04-20',
                              'authorization': 'Bearer ' + TOKEN},
                   'usage': {'cache_creation_input_tokens': 256, 'cache_read_input_tokens': 0, 'output_tokens': 2}}
    add_text('beta-flags-and-usage', diagnostics, {'/headers/authorization': [TOKEN]},
             'An agent inspects request feature flags and token-usage counters beside an Authorization header.',
             'Keep feature flag names, usage keys and numeric values; protect only the credential in the header.',
             evidence([99, 69]), validity=[{'format': 'json-value', 'pointer': ''}])
    urls = ('Public observations: https://data.example.invalid/2026/09/daily-observations-20260923.json?station=WEST-42&format=json\n'
            f'Private request: https://api.example.invalid/report?api_key={TOKEN}\n')
    add_text('public-reference-urls', urls, [TOKEN],
             'An agent cites a public data URL and a separate authenticated endpoint in a research handoff.',
             'Retain the public URL and ordinary query parameters under entropy opt-in; protect the credential-bearing query value without exempting all URLs.',
             evidence([69], 'coffee', 'The frozen public session visibly contains V1 placeholders in call_id fields and strings shaped as https: followed by a placeholder. Original hidden URL bytes are unavailable; the URLs here are newly constructed public-reference examples.'),
             entropy=True, validity=[{'format': 'reference-urls', 'pointer': ''}])

    def add_policy(name, records, checks, scenario, expectation, provenance, **options):
        policy('incident-' + name, records, checks, expectation, mode='plain',
               coverage_kind='incident-reconstruction', scenario=scenario, evidence=provenance,
               expectation_basis='docs/secret-context-classification.md; docs/privacy-plain-publication.md',
               source='Synthetic native records preserve only the reported failure shape; IDs, paths, credentials and media are replaced.',
               **options)

    uuid = '7ac42168-239d-4e75-96b1-5c2e803fa947'
    session = '6e52a197-3d84-4fb6-b8c2-91a765df203e'
    call_id = 'toolu_01Vn8Qm4Rz6Kb2Hs9Wx5Et7P'
    text = 'Copied private correlation value: ' + uuid
    records = [
        {'type': 'assistant', 'uuid': uuid, 'sessionId': session, 'message': {'role': 'assistant', 'content': [
            {'type': 'tool_use', 'id': call_id, 'name': 'Bash', 'input': {'command': "printf '%s\\n' '" + uuid + "'"}}]}},
        {'type': 'user', 'uuid': '4e1703b9-c628-4d5f-91a7-68b2053dce42', 'parentUuid': uuid, 'sessionId': session,
         'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': call_id, 'content': text}]}},
    ]
    command = records[0]['message']['content'][0]['input']['command']
    add_policy('claude-native-identities', records,
        [check('/records/0/uuid', 'keep', uuid), check('/records/1/parentUuid', 'keep', uuid),
         check('/records/0/sessionId', 'keep', session), check('/records/1/sessionId', 'keep', session),
         check('/records/0/message/content/0/id', 'keep', call_id), check('/records/1/message/content/0/tool_use_id', 'keep', call_id),
         check('/records/0/message/content/0/input/command', 'rewrite', command, replacement_pattern=protected_pattern(command, uuid)),
         check('/records/1/message/content/0/content', 'rewrite', text, replacement_pattern=protected_pattern(text, uuid))],
        'A native Claude call/result pair carries UUID linkage fields, while the same UUID is explicitly blocked as ordinary command/result content.',
        'Preserve verified native identifiers even under entropy opt-in and an explicit block; protect the same value in ordinary content. Use the current protocol contract, not the older issue proposal.',
        evidence([69, 99]), entropy=True, blocks=[uuid])

    call_id = 'call_V9qB3nR7mK2xH8pL4sW6cF1a'
    arguments = json.dumps({'cmd': "printf '%s\\n' '" + call_id + "'", 'workdir': '/workspace/demo'}, separators=(',', ':'), sort_keys=True)
    text = 'Private correlation value: ' + call_id
    records = [
        {'type': 'response_item', 'payload': {'type': 'function_call', 'name': 'exec_command', 'call_id': call_id, 'arguments': arguments}},
        {'type': 'response_item', 'payload': {'type': 'function_call_output', 'call_id': call_id, 'output': text}},
    ]
    layout = [
        {'role': 'assistant', 'preserve': ['/records/0/type', '/records/0/payload/type'], 'blocks': [{'type': 'tool_use', 'fields': {
            'id': binding('/records/0/payload/call_id'), 'name': binding('/records/0/payload/name'), 'input': binding('/records/0/payload/arguments', True)}}]},
        {'role': 'user', 'preserve': ['/records/1/type', '/records/1/payload/type'], 'blocks': [{'type': 'tool_result', 'fields': {
            'tool_use_id': binding('/records/1/payload/call_id'), 'content': binding('/records/1/payload/output')}}]},
    ]
    add_policy('codex-call-linkage', records,
        [check('/records/0/payload/call_id', 'keep', call_id), check('/records/1/payload/call_id', 'keep', call_id),
         check('/records/0/payload/arguments', 'rewrite', arguments, replacement_pattern=protected_pattern(arguments, call_id)),
         check('/records/1/payload/output', 'rewrite', text, replacement_pattern=protected_pattern(text, call_id))],
        'A Codex function call uses a native call_id which also occurs as an explicitly blocked string inside its JSON-encoded command and result.',
        'Keep call_id byte-for-byte and preserve the pairing while protecting the content occurrence; no placeholder may be written into native linkage.',
        evidence([69, 123], 'coffee', 'V1 placeholders are directly visible in the frozen session call_id fields. This fixture uses new synthetic IDs to test preservation rather than replaying the broken historical IDs.'),
        runtime='codex', record_layout=layout, entropy=True, blocks=[call_id], validity=[{'format': 'json', 'pointer': '/records/0/payload/arguments'}])

    image = base64.b64encode(texture_png()).decode()
    text = 'The preview renderer returned this temporary API token: ' + TOKEN
    records = [
        {'type': 'assistant', 'message': {'role': 'assistant', 'content': [
            {'type': 'tool_use', 'id': 'toolu_01ImageReadProbe', 'name': 'Read', 'input': {'file_path': '/workspace/demo/texture.png'}}]}},
        {'type': 'user', 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': 'toolu_01ImageReadProbe', 'content': [
            {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': image}},
            {'type': 'text', 'text': text}]}]},
         'toolUseResult': {'type': 'image', 'file': {'base64': image, 'type': 'image/png'}}},
    ]
    image_pointer = '/records/1/message/content/0/content/0/source/data'
    copy_pointer = '/records/1/toolUseResult/file/base64'
    add_policy('claude-duplicated-image', records,
        [check(image_pointer, 'keep', image), check(copy_pointer, 'keep', image),
         check('/records/1/message/content/0/content/1/text', 'rewrite', text, replacement_pattern=protected_pattern(text, TOKEN))],
        'A Claude Read result stores the same small PNG in a structured image block and the native toolUseResult companion, beside diagnostic text.',
        'Preserve both valid image copies under entropy opt-in and protect the adjacent credential; media evidence must not exempt surrounding text.',
        evidence([71, 62], 'ppt', 'The frozen Hub session corroborates the image-to-PowerPoint workflow. The duplicate native carrier paths and import refusal are established by issue 71; rendered Hub events do not prove raw carrier equality.'),
        entropy=True, validity=[{'format': 'png-base64', 'pointer': image_pointer},
                               {'format': 'png-base64', 'pointer': copy_pointer, 'same_as': image_pointer}])
    mcp = {'mcp_result': {'content': [{'type': 'image', 'mimeType': 'image/png', 'data': image}, {'type': 'text', 'text': text}]},
           'rendered_input': {'type': 'input_image', 'image_url': 'data:image/png;base64,' + image}}
    add_text('mcp-image-carrier-equivalence', mcp, {'/mcp_result/content/1/text': [TOKEN]},
             'A tool diagnostic retains both an MCP image result and the equivalent data URL prepared for the model.',
             'Preserve equivalent valid image bytes in both carriers under entropy opt-in and protect the text credential. This isolates media classification, not the import size budget.',
             evidence([62, 71], 'coffee', 'Issue 62 identifies raw MCP data versus an equivalent data URL. The accessible session is already protected history, so original image bytes and the earlier import rejection are not independently recovered here.'),
             entropy=True, validity=[{'format': 'png-base64', 'pointer': '/mcp_result/content/0/data'},
                                     {'format': 'png-data-url', 'pointer': '/rendered_input/image_url', 'same_image_as': '/mcp_result/content/0/data'}])

    legacy = '{{AGIT_SECRET_V1:10000000-0000-4000-8000-000000000001:sec_10000000000000000000000000000002}}'
    foreign = '{{AGIT_SECRET_V2:20000000-0000-4000-8000-000000000001:20000000-0000-4000-8000-000000000002}}'
    note = f'A teammate shared already-protected history: {legacy} and {foreign}.\nThe new deployment credential is {TOKEN}.\n'
    add_policy('foreign-placeholder-literals', [message(note)],
        [check('/records/0/message/content', 'rewrite', note, replacement_pattern=protected_pattern(note, TOKEN))],
        'A user quotes already-protected history from another dictionary alongside a newly pasted credential.',
        'Keep unknown canonical placeholders literally and protect the new credential. This checks capture only, not opening-request hydration or cross-device synchronization.',
        evidence([60, 353], 'foreign', 'The frozen session user prompt directly reports publication hydration is incomplete. Issue 353 distinguishes foreign literals from locally held but unavailable records; this case covers only the foreign-literal input boundary.'))
