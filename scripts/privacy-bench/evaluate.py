"""Score independently annotated ranges, without treating noncompletion as correctness."""
import collections
import json

from adapter import TOKEN, dumps

STATUSES = ('complete', 'failed', 'partial', 'unsupported', 'indeterminate')


def pointer(value, path):
    for part in path.split('/')[1:]:
        key = part.replace('~1', '/').replace('~0', '~')
        value = value[int(key)] if isinstance(value, list) else value[key]
    return value


def leaves(value, path=''):
    if isinstance(value, (list, dict)):
        for key, child in (enumerate(value) if isinstance(value, list) else value.items()):
            yield from leaves(child, path + '/' + str(key).replace('~', '~0').replace('/', '~1'))
    else:
        yield path, value


def align(original, output, mappings):
    """Recovery proves each replaced range; unexplained edits cannot be guessed from a diff."""
    recovered = []
    spans = []
    cursor = 0
    offset = 0
    for match in TOKEN.finditer(output):
        literal = output[cursor:match.start()]
        recovered.append(literal)
        offset += len(literal)
        if match.group() not in mappings:
            raise ValueError('Unknown placeholder')
        value = mappings[match.group()]
        recovered.append(value)
        spans.append((offset, offset + len(value)))
        offset += len(value)
        cursor = match.end()
    recovered.append(output[cursor:])
    if ''.join(recovered) != original:
        raise ValueError('Output cannot be aligned unambiguously through recovery')
    return spans


def coverage(start, end, spans):
    cursor = start
    for left, right in sorted(set(spans)):
        if left <= cursor < right:
            cursor = right
    return cursor >= end


def overlap(start, end, spans):
    return any(left < end and start < right for left, right in spans)


def score(case, annotations, result):
    result = dict(result)
    result['checks'] = []
    result['counts'] = {key: 0 for key in ('TP', 'FP', 'FN', 'TN')}
    result['contract_failures'] = [name for name, passed in result.get('contracts', {}).items() if not passed]
    if result['status'] != 'complete':
        if result['status'] not in STATUSES:
            result['status'] = 'failed'
        return result
    try:
        spans = {}
        if case['layer'] == 'secret':
            before = dict(leaves(case['input']))
            after = dict(leaves(result['output']))
            if before.keys() != after.keys() or any(type(v) is not type(after[p]) for p, v in before.items()):
                raise ValueError('Output JSON structure or types changed')
            for path, original in before.items():
                if isinstance(original, str):
                    spans[path] = align(original, after[path], result.get('mappings', {}))
                elif original != after[path]:
                    result['contract_failures'].append('unchanged_scalar:' + path)
        for index, annotation in enumerate(annotations):
            expected = annotation['expect']
            path = annotation['pointer']
            if case['layer'] == 'secret':
                start, end = annotation['start'], annotation['end']
                if expected == 'protect':
                    verdict = 'TP' if coverage(start, end, spans[path]) else 'FN'
                else:
                    verdict = 'FP' if overlap(start, end, spans[path]) else 'TN'
            else:
                try:
                    actual = pointer(result['output'], path)
                except (KeyError, IndexError, TypeError, ValueError):
                    actual = None
                actual_text = actual if isinstance(actual, str) else dumps(actual)
                present = annotation['value'] in actual_text
                if expected == 'exclude':
                    verdict = 'TP' if actual == annotation['value'] else 'FN'
                elif expected == 'allow':
                    verdict = 'TN' if actual == annotation['value'] else 'FP'
                elif expected == 'hide':
                    verdict = 'FN' if present else 'TP'
                elif expected == 'keep':
                    verdict = 'TN' if present else 'FP'
                else:
                    passed = not present and annotation['replacement'] in actual_text
                    verdict = 'rewrite_pass' if passed else 'rewrite_fail'
                    if not passed:
                        result['contract_failures'].append(f'rewrite:{index}')
            result['checks'].append({'index': index, **annotation, 'verdict': verdict})
            if verdict in result['counts']:
                result['counts'][verdict] += 1
    except (ValueError, KeyError, TypeError, IndexError) as e:
        result['status'] = 'indeterminate'
        result['reason'] = str(e)
        result['checks'] = []
        result['counts'] = {key: 0 for key in result['counts']}
    if result['status'] == 'complete' and result.get('detection'):
        try:
            raw = case['input'].encode()
            findings = result['detection']['findings']
            if not all(isinstance(f['start'], int) and isinstance(f['end'], int)
                       and 0 <= f['start'] < f['end'] <= len(raw) for f in findings):
                raise ValueError('Invalid detector range')
            detected = [(len(raw[:f['start']].decode()), len(raw[:f['end']].decode())) for f in findings]
            counts = dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0)
            for check in annotations:
                a, b = check['start'], check['end']
                if check['expect'] == 'protect':
                    verdict = 'TP' if coverage(a, b, detected) else 'FN'
                else:
                    verdict = 'FP' if overlap(a, b, detected) else 'TN'
                counts[verdict] += 1
            result['detection_counts'] = counts
        except (ValueError, KeyError, TypeError):
            result['contract_failures'].append('invalid_detector_ranges')
    return result


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def summarize(cases, results, ids=None):
    selected = [c for c in cases if ids is None or c['id'] in ids]
    summary = {'completion': dict.fromkeys(STATUSES, 0), 'total': len(selected)}
    for case in selected:
        summary['completion'][results[case['id']]['status']] += 1
    for layer in ('secret', 'policy'):
        counts = collections.Counter(dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0))
        normal = normal_fp = contract_failures = rewrites = rewrite_failures = 0
        for case in selected:
            result = results[case['id']]
            if case['layer'] != layer or result['status'] != 'complete':
                continue
            counts.update(result['counts'])
            contract_failures += len(result['contract_failures'])
            rewrites += sum(c['verdict'].startswith('rewrite_') for c in result['checks'])
            rewrite_failures += sum(c['verdict'] == 'rewrite_fail' for c in result['checks'])
            if layer == 'secret' and not any(c['expect'] == 'protect' for c in result['checks']):
                normal += 1
                normal_fp += result['counts']['FP'] > 0
        summary[layer] = {**counts, 'precision': ratio(counts['TP'], counts['TP'] + counts['FP']),
                          'recall': ratio(counts['TP'], counts['TP'] + counts['FN']),
                          'false_negative_rate': ratio(counts['FN'], counts['TP'] + counts['FN']),
                          'normal_cases': normal, 'normal_cases_with_fp': normal_fp,
                          'normal_case_fp_rate': ratio(normal_fp, normal), 'contract_failures': contract_failures,
                          'rewrite_checks': rewrites, 'rewrite_failures': rewrite_failures}
    summary['secret_modes'] = {}
    for mode in ('default', 'entropy'):
        grouped = [c for c in selected if c['layer'] == 'secret' and bool(c.get('entropy')) == (mode == 'entropy')]
        counts = collections.Counter(dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0))
        completion = dict.fromkeys(STATUSES, 0)
        for case in grouped:
            r = results[case['id']]
            completion[r['status']] += 1
            if r['status'] == 'complete':
                counts.update(r['counts'])
        summary['secret_modes'][mode] = {**counts, 'completion': completion,
            'precision': ratio(counts['TP'], counts['TP'] + counts['FP']),
            'recall': ratio(counts['TP'], counts['TP'] + counts['FN']),
            'false_negative_rate': ratio(counts['FN'], counts['TP'] + counts['FN'])}
    detection = collections.Counter(dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0))
    available = 0
    for case in selected:
        result = results[case['id']]
        if result['status'] == 'complete' and 'detection_counts' in result:
            detection.update(result['detection_counts'])
            available += 1
    summary['detector_telemetry'] = {**detection, 'completed_cases': available,
        'unavailable_cases': sum(c['layer'] == 'secret' for c in selected) - available,
        'precision': ratio(detection['TP'], detection['TP'] + detection['FP']),
        'recall': ratio(detection['TP'], detection['TP'] + detection['FN'])}
    return summary


def compare(cases, baseline, candidate):
    common = [c['id'] for c in cases if baseline[c['id']]['status'] == candidate[c['id']]['status'] == 'complete']
    changes = {name: [] for name in ('new_FP', 'new_FN', 'fixed_FP', 'fixed_FN', 'new_contract_failures', 'lost_coverage')}
    for case in cases:
        name = case['id']
        old, new = baseline[name], candidate[name]
        if old['status'] == 'complete' and new['status'] != 'complete':
            changes['lost_coverage'].append(name)
        if name not in common:
            continue
        for before, after in zip(old['checks'], new['checks'], strict=True):
            for kind in ('FP', 'FN'):
                detail = {'case': name, 'annotation': before['index']}
                if before['verdict'] != kind and after['verdict'] == kind:
                    changes['new_' + kind].append(detail)
                if before['verdict'] == kind and after['verdict'] != kind:
                    changes['fixed_' + kind].append(detail)
        if set(new['contract_failures']) - set(old['contract_failures']):
            changes['new_contract_failures'].append(name)
    return {'common_completed_ids': common, 'changes': changes,
            'common': {'baseline': summarize(cases, baseline, common), 'candidate': summarize(cases, candidate, common)},
            'all': {'baseline': summarize(cases, baseline), 'candidate': summarize(cases, candidate)}}


def succeeds(cases, baseline, candidate):
    if any(r['status'] != 'complete' for r in (*baseline.values(), *candidate.values())):
        return False
    return all(not (r['counts']['FP'] or r['counts']['FN'] or r['contract_failures']
                    or r.get('detection_counts', {}).get('FP') or r.get('detection_counts', {}).get('FN')) for r in candidate.values())
