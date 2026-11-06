"""Independent input contracts and structural checks shared by the runner and adapter."""
import json
from pathlib import PurePosixPath
import re

from sources import BenchError

MISSING = object()
RUNTIMES = ('claude-code', 'claude-desktop', 'cursor', 'codex', 'opencode', 'hermes', 'openclaw', 'workbuddy')


def validate_workspace_files(files):
    if not isinstance(files, dict):
        raise ValueError('Workspace files must map relative paths to text')
    for path, content in files.items():
        if (not isinstance(path, str) or not path or '\\' in path or ':' in path or '\x00' in path
                or PurePosixPath(path).is_absolute() or any(part in ('', '.', '..', '.git') for part in path.split('/'))
                or not isinstance(content, str)):
            raise ValueError('Workspace fixture must stay inside the synthetic working tree')


def pointer_parts(path):
    if not isinstance(path, str) or (path and not path.startswith('/')):
        raise ValueError('Invalid JSON pointer')
    if re.search(r'~(?![01])', path):
        raise ValueError('Invalid JSON pointer escape')
    return [part.replace('~1', '/').replace('~0', '~') for part in path.split('/')[1:]]


def pointer(value, path):
    for part in pointer_parts(path):
        if isinstance(value, list):
            if not re.fullmatch(r'0|[1-9][0-9]*', part):
                raise ValueError('Invalid array index')
            value = value[int(part)]
        elif isinstance(value, dict):
            value = value[part]
        else:
            raise TypeError('Pointer traverses a scalar')
    return value


def lookup(value, path):
    try:
        return pointer(value, path)
    except (KeyError, IndexError, TypeError):
        return MISSING


def leaves(value, path=''):
    if isinstance(value, (list, dict)):
        for key, child in (enumerate(value) if isinstance(value, list) else value.items()):
            yield from leaves(child, path + '/' + str(key).replace('~', '~0').replace('/', '~1'))
    else:
        yield path, value


def same_shape(before, after):
    if type(before) is not type(after):
        return False
    if isinstance(before, dict):
        return before.keys() == after.keys() and all(same_shape(v, after[k]) for k, v in before.items())
    if isinstance(before, list):
        return len(before) == len(after) and all(same_shape(a, b) for a, b in zip(before, after))
    return True


def exact(before, after):
    return same_shape(before, after) and before == after


def layout_records(case, value, decoding_errors=None):
    """Resolve reviewed native-field bindings for structural scoring, never for worker input."""
    layout = case['record_layout']
    if not isinstance(layout, list) or len(layout) != len(value['records']):
        raise ValueError('Record layout must describe each native record')
    records = []
    for index, row in enumerate(layout):
        if (not isinstance(row, dict) or set(row) != {'role', 'blocks', 'preserve'} or row['role'] not in ('user', 'assistant')
                or not isinstance(row['blocks'], list) or not row['blocks'] or not isinstance(row['preserve'], list)
                or not row['preserve']):
            raise ValueError('Invalid record layout')
        for path in row['preserve']:
            if not isinstance(path, str) or not path.startswith(f'/records/{index}/'):
                raise ValueError('Protocol field must identify its own record')
            pointer(value, path)
        blocks = []
        for block in row['blocks']:
            if (not isinstance(block, dict) or set(block) != {'type', 'fields'}
                    or not isinstance(block['type'], str) or not isinstance(block['fields'], dict)):
                raise ValueError('Invalid block layout')
            resolved = {'type': block['type']}
            for name, binding in block['fields'].items():
                if (name == 'type' or not isinstance(binding, dict) or set(binding) - {'pointer', 'encoding'}
                        or binding.get('encoding', 'value') not in ('value', 'json')):
                    raise ValueError('Invalid native-field binding')
                if not isinstance(binding.get('pointer'), str) or not binding['pointer'].startswith(f'/records/{index}/'):
                    raise ValueError('Native-field binding must identify its own record')
                field = pointer(value, binding['pointer'])
                if binding.get('encoding') == 'json':
                    try:
                        resolved[name] = json.loads(field)
                    except (ValueError, TypeError):
                        if decoding_errors is None:
                            raise
                        decoding_errors.append(binding['pointer'])
                        resolved[name] = field
                else:
                    resolved[name] = field
            required = {'tool_use': {'id', 'name', 'input'}, 'tool_result': {'tool_use_id', 'content'},
                        'text': {'text'}}.get(block['type'], set())
            if not required <= resolved.keys():
                raise ValueError('Layout is missing structural fields')
            blocks.append(resolved)
        records.append({'type': row['role'], 'message': {'role': row['role'], 'content': blocks}})
    return records


def policy_record_contracts(case, before, after):
    expected = layout_records(case, before)
    if case['mode'] == 'plain':
        decoding_errors = []
        contracts = record_contracts(expected, layout_records(case, after, decoding_errors), plain=True)
        contracts['encoded_native_fields'] = not decoding_errors
        contracts['native_structure'] = same_shape(before['records'], after['records'])
        contracts['native_protocol_fields'] = all(exact(pointer(before, path), pointer(after, path))
            for row in case['record_layout'] for path in row['preserve'])
        return contracts
    return record_contracts(expected, after['records'])


def protected_ranges(original, output, mappings, token_pattern):
    recovered, spans = [], []
    cursor = offset = 0
    for match in token_pattern.finditer(output):
        literal = output[cursor:match.start()]
        recovered.append(literal)
        offset += len(literal)
        value = mappings[match.group()]
        recovered.append(value)
        spans.append((offset, offset + len(value)))
        offset += len(value)
        cursor = match.end()
    recovered.append(output[cursor:])
    if ''.join(recovered) != original:
        raise ValueError('Output cannot be aligned unambiguously through recovery')
    return spans


def validate_stream(case):
    if 'context' in case and not isinstance(case['context'], str):
        raise ValueError('Context must be text')
    if 'stream_cuts' not in case:
        return
    cuts = case['stream_cuts']
    if case.get('layer') != 'secret' or not isinstance(case['input'], str):
        raise ValueError('Stream cuts require a Secret text case')
    if not isinstance(cuts, list) or not cuts:
        raise ValueError('Stream cuts must be a nonempty list of Unicode code-point offsets')
    if any(type(cut) is not int or not 0 < cut < len(case['input']) for cut in cuts):
        raise ValueError('Stream cut outside the text')
    if cuts != sorted(set(cuts)):
        raise ValueError('Stream cuts must be strictly increasing')


def validate_case(case, checks):
    name = case.get('id', '<missing>')
    index = None
    try:
        if not isinstance(name, str) or not re.fullmatch(r'[a-z0-9][a-z0-9-]*', name):
            raise ValueError('Invalid case ID')
        if case['layer'] not in ('secret', 'policy') or not isinstance(checks, list) or not checks:
            raise ValueError('Case needs a known layer and independent expectations')
        if case['layer'] == 'policy' and case.get('mode') not in ('plain', 'encrypted_public', 'privacy_export', 'memory'):
            raise ValueError('Unknown Policy mode')
        if 'branch' in case and (case['layer'] != 'policy' or case['mode'] == 'memory'
                                  or not isinstance(case['branch'], str) or not case['branch']):
            raise ValueError('Branch selection requires a nonempty publication branch name')
        if 'workspace_files' in case:
            if case['layer'] != 'policy' or case['mode'] == 'memory':
                raise ValueError('Workspace fixtures require a publication case')
            validate_workspace_files(case['workspace_files'])
        if case['layer'] == 'policy' and case['mode'] != 'memory':
            if case.get('runtime') not in RUNTIMES:
                raise ValueError('Policy case must declare a supported runtime')
            if case['input'].get('metadata', {}).get('runtime', case['runtime']) != case['runtime']:
                raise ValueError('Metadata runtime conflicts with the case runtime')
            semantic = layout_records(case, case['input'])
            if not all(record_contracts(semantic, semantic, plain=True).values()):
                raise ValueError('Native record layout contains invalid tool associations')
        validate_stream(case)
        locations = dict(leaves(case['input']))
        for index, check in enumerate(checks):
            pointer_parts(check['pointer'])
            expected = check['expect']
            if case['layer'] == 'secret':
                if set(check) != {'pointer', 'start', 'end', 'expect'} or expected not in ('protect', 'keep'):
                    raise ValueError('Invalid Secret annotation fields or expectation')
                value = locations[check['pointer']]
                if (not isinstance(value, str) or type(check['start']) is not int or type(check['end']) is not int
                        or not 0 <= check['start'] < check['end'] <= len(value)):
                    raise ValueError('Invalid Secret range')
                continue
            allowed = {'pointer', 'expect', 'value', 'input_pointer', 'replacement', 'replacement_pattern'}
            if set(check) - allowed or expected not in ('hide', 'keep', 'rewrite', 'remove', 'allow', 'exclude'):
                raise ValueError('Invalid Policy annotation fields or expectation')
            if expected in ('allow', 'exclude'):
                if set(check) != {'pointer', 'expect', 'value'} or case.get('mode') != 'memory':
                    raise ValueError('Memory decisions require exact action expectations')
                parts = pointer_parts(check['pointer'])
                if (len(parts) != 3 or parts[0] != 'decisions' or parts[2] != 'action'
                        or not re.fullmatch(r'0|[1-9][0-9]*', parts[1])
                        or int(parts[1]) >= len(case['paths'])
                        or check['value'] != {'allow': 'allowed', 'exclude': 'excluded'}[expected]):
                    raise ValueError('Invalid memory decision target')
                continue
            source = pointer(case['input'], check['input_pointer'])
            if not exact(source, check['value']):
                raise ValueError('Policy input target does not equal its declared value')
            replacements = int('replacement' in check) + int('replacement_pattern' in check)
            if expected in ('hide', 'rewrite'):
                if replacements != 1:
                    raise ValueError('Hide and rewrite require an exact replacement or full-match pattern')
                if expected == 'hide' and (not isinstance(source, str) or not source):
                    raise ValueError('Hide requires a nonempty sensitive string')
                if 'replacement_pattern' in check:
                    re.compile(check['replacement_pattern'])
            elif replacements:
                raise ValueError('Unexpected replacement')
            if expected == 'remove' and not check['pointer']:
                raise ValueError('Cannot remove the output root')
            if expected in ('hide', 'rewrite') and 'replacement' in check and exact(source, check['replacement']):
                raise ValueError('Protection must change its target')
        if case['layer'] == 'secret':
            index = None
            for path, value in locations.items():
                if not isinstance(value, str):
                    continue
                cursor = 0
                for check in sorted((c for c in checks if c['pointer'] == path), key=lambda c: c['start']):
                    if check['start'] != cursor:
                        raise ValueError('Secret ranges must partition each string without gaps or overlap')
                    cursor = check['end']
                if cursor != len(value):
                    raise ValueError('Unannotated input string: ' + path)
    except (ValueError, KeyError, TypeError, IndexError, re.error) as error:
        location = f'{name}, annotation {index}' if index is not None else name
        raise BenchError(f'Invalid dataset ({location}): {error}') from error


def record_contracts(before, after, plain=False):
    """Public IDs may change, but each result must follow its original call at the same location."""
    contracts = {'records_preserved': isinstance(after, list) and len(before) == len(after),
                 'record_structure': False, 'paired_calls': False}
    try:
        if not contracts['records_preserved']:
            return contracts
        if plain and not same_shape(before, after):
            return contracts
        calls, references, aliases = {}, [], set()
        for original, output in zip(before, after, strict=True):
            if (original['type'] != output['type'] or original['message']['role'] != output['message']['role']
                    or output['type'] != output['message']['role']):
                return contracts
            if not plain and (set(output) != {'type', 'message'} or set(output['message']) != {'role', 'content'}):
                return contracts
            old_blocks = original['message']['content']
            if isinstance(old_blocks, str):
                old_blocks = [{'type': 'text', 'text': old_blocks}]
            new_blocks = output['message']['content']
            if plain and isinstance(new_blocks, str):
                new_blocks = [{'type': 'text', 'text': new_blocks}]
            if not isinstance(new_blocks, list) or len(old_blocks) != len(new_blocks):
                return contracts
            for old, new in zip(old_blocks, new_blocks, strict=True):
                kind = old['type']
                if not isinstance(new, dict) or not isinstance(new.get('type'), str):
                    return contracts
                if kind in ('text', 'tool_use', 'tool_result') and new['type'] != kind:
                    return contracts
                if kind == 'text' and (set(new) != {'type', 'text'} or not isinstance(new['text'], str)):
                    return contracts
                if kind == 'tool_use':
                    if set(new) != set(old) or not isinstance(new.get('name'), str) or not isinstance(new.get('input'), (dict, str)):
                        return contracts
                    if new['name'] != 'omitted' and (new['name'] != old['name'] or not same_shape(old['input'], new['input'])):
                        return contracts
                    identity, alias = old['id'], new['id']
                    if (not isinstance(alias, str) or not alias or identity in calls or alias in aliases
                            or (plain and identity != alias)):
                        return contracts
                    calls[identity] = alias
                    aliases.add(alias)
                elif kind == 'tool_result':
                    expected_keys = set(old) if plain else set(old) | {'is_error'}
                    if set(new) != expected_keys or not exact(new.get('is_error', False), old.get('is_error', False)):
                        return contracts
                    references.append((old['tool_use_id'], new['tool_use_id']))
        contracts['record_structure'] = True
        contracts['paired_calls'] = all(identity in calls and calls[identity] == alias for identity, alias in references)
    except (KeyError, TypeError, ValueError):
        pass
    return contracts
