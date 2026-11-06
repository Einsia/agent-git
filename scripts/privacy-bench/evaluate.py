"""Score independently annotated ranges, without treating noncompletion as correctness."""
import collections
import re

from adapter import TOKEN, dumps
from contracts import MISSING, exact, leaves, lookup, protected_ranges, same_shape

STATUSES = ('complete', 'failed', 'partial', 'unsupported', 'indeterminate')


def align(original, output, mappings):
    """Recovery proves each replaced range; unexplained edits cannot be guessed from a diff."""
    return protected_ranges(original, output, mappings, TOKEN)


def coverage(start, end, spans):
    cursor = start
    for left, right in sorted(set(spans)):
        if left <= cursor < right:
            cursor = right
    return cursor >= end


def overlap(start, end, spans):
    return any(left < end and start < right for left, right in spans)


def score_detection(case, annotations, result):
    result['detection_checks'] = []
    result.pop('detection_counts', None)
    detection = result.get('detection')
    if detection is None:
        result.setdefault('detection_status', 'not_applicable')
        return
    try:
        if (not isinstance(detection, dict) or detection.get('schema_version') != 1
                or detection.get('status') not in ('complete', 'partial', 'failed')):
            raise ValueError('Invalid detector response')
        result['detection_status'] = detection['status']
        if detection['status'] != 'complete':
            result['detection_reason'] = detection.get('reason', 'Detector did not complete')
            return
        if detection.get('offset_unit') != 'utf8_bytes':
            raise ValueError('Invalid detector coordinate unit')
        raw = case['input'].encode()
        findings = detection['findings']
        if not isinstance(findings, list) or not all(
                isinstance(f, dict) and type(f.get('start')) is int and type(f.get('end')) is int
                and 0 <= f['start'] < f['end'] <= len(raw) for f in findings):
            raise ValueError('Invalid detector range')
        detected = [(len(raw[:f['start']].decode()), len(raw[:f['end']].decode())) for f in findings]
        counts = dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0)
        for index, check in enumerate(annotations):
            a, b = check['start'], check['end']
            if check['expect'] == 'protect':
                verdict = 'TP' if coverage(a, b, detected) else 'FN'
            else:
                verdict = 'FP' if overlap(a, b, detected) else 'TN'
            counts[verdict] += 1
            result['detection_checks'].append({'index': index, **check, 'verdict': verdict})
        result['detection_counts'] = counts
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        result['detection_status'] = 'failed'
        result['detection_reason'] = str(error)


def replacement_matches(actual, annotation):
    if 'replacement_pattern' in annotation:
        return isinstance(actual, str) and re.fullmatch(annotation['replacement_pattern'], actual) is not None
    return exact(actual, annotation['replacement'])


def score(case, annotations, result):
    result = dict(result)
    result['checks'] = []
    result['counts'] = {key: 0 for key in ('TP', 'FP', 'FN', 'TN')}
    result['contract_failures'] = [name for name, passed in result.get('contracts', {}).items() if not passed]
    score_detection(case, annotations, result)
    if result['status'] != 'complete':
        if result['status'] not in STATUSES:
            result['status'] = 'failed'
        return result
    try:
        spans = {}
        if case.get('mode') == 'memory':
            decisions = result['output'].get('decisions')
            if (not isinstance(decisions, list) or len(decisions) != len(case['paths'])
                    or any(not isinstance(d, dict) or d.get('input') != path
                           for d, path in zip(decisions, case['paths']))):
                result['contract_failures'].append('memory_decision_structure')
                raise ValueError('Memory decisions do not match the input paths')
        if case['layer'] == 'secret':
            before = dict(leaves(case['input']))
            if not same_shape(case['input'], result['output']):
                result['contract_failures'].append('json_structure')
                raise ValueError('Output JSON structure or types changed')
            after = dict(leaves(result['output']))
            for path, original in before.items():
                if isinstance(original, str):
                    spans[path] = align(original, after[path], result.get('mappings', {}))
                elif not exact(original, after[path]):
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
                actual = lookup(result['output'], path)
                if expected == 'remove':
                    parent = lookup(result['output'], path.rsplit('/', 1)[0])
                    passed = actual is MISSING and isinstance(parent, (dict, list))
                    verdict = 'TP' if passed else 'FN'
                    if not passed:
                        result['contract_failures'].append(f'remove:{index}')
                elif actual is MISSING:
                    verdict = 'FP' if expected in ('keep', 'allow') else 'FN' if expected in ('hide', 'exclude') else 'rewrite_fail'
                    result['contract_failures'].append(f'missing_target:{index}')
                elif expected == 'exclude':
                    verdict = 'TP' if exact(actual, annotation['value']) else 'FN'
                elif expected in ('allow', 'keep'):
                    verdict = 'TN' if exact(actual, annotation['value']) else 'FP'
                elif expected == 'hide':
                    present = annotation['value'] in (actual if isinstance(actual, str) else dumps(actual))
                    verdict = 'FN' if present else 'TP'
                    if not replacement_matches(actual, annotation):
                        result['contract_failures'].append(f'hide_replacement:{index}')
                elif expected == 'rewrite':
                    passed = replacement_matches(actual, annotation)
                    verdict = 'rewrite_pass' if passed else 'rewrite_fail'
                    if not passed:
                        result['contract_failures'].append(f'rewrite:{index}')
                else:
                    raise ValueError('Unknown Policy expectation')
            result['checks'].append({'index': index, **annotation, 'verdict': verdict})
            if verdict in result['counts']:
                result['counts'][verdict] += 1
    except (ValueError, KeyError, TypeError, IndexError) as error:
        result['status'] = 'indeterminate'
        result['reason'] = str(error)
        result['checks'] = []
        result['counts'] = {key: 0 for key in result['counts']}
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
            if case['layer'] != layer:
                continue
            contract_failures += len(result['contract_failures'])
            if result['status'] != 'complete':
                continue
            counts.update(result['counts'])
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
    return summary


def summarize_detection(cases, results, ids=None):
    selected = [c for c in cases if c['layer'] == 'secret' and (ids is None or c['id'] in ids)]
    counts = collections.Counter(dict.fromkeys(('TP', 'FP', 'FN', 'TN'), 0))
    completion = collections.Counter()
    for case in selected:
        result = results[case['id']]
        completion[result.get('detection_status', 'not_applicable')] += 1
        if result.get('detection_status') == 'complete':
            counts.update(result['detection_counts'])
    return {**counts, 'completion': dict(completion), 'total': len(selected),
            'completed_cases': completion['complete'], 'unavailable_cases': len(selected) - completion['complete'],
            'precision': ratio(counts['TP'], counts['TP'] + counts['FP']),
            'recall': ratio(counts['TP'], counts['TP'] + counts['FN'])}


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
    detector_common = [c['id'] for c in cases if baseline[c['id']].get('detection_status')
                       == candidate[c['id']].get('detection_status') == 'complete']
    detector_lost = [c['id'] for c in cases if baseline[c['id']].get('detection_status') == 'complete'
                     and candidate[c['id']].get('detection_status') != 'complete']
    return {'common_completed_ids': common, 'changes': changes,
            'detection': {'common_completed_ids': detector_common, 'lost_coverage': detector_lost,
                          'common': {'baseline': summarize_detection(cases, baseline, detector_common),
                                     'candidate': summarize_detection(cases, candidate, detector_common)},
                          'all': {'baseline': summarize_detection(cases, baseline),
                                  'candidate': summarize_detection(cases, candidate)}},
            'common': {'baseline': summarize(cases, baseline, common), 'candidate': summarize(cases, candidate, common)},
            'all': {'baseline': summarize(cases, baseline), 'candidate': summarize(cases, candidate)}}


def succeeds(cases, baseline, candidate):
    expected = {c['id'] for c in cases}
    if set(baseline) != expected or set(candidate) != expected:
        return False
    if any(r['status'] != 'complete' or r.get('detection_status') in ('failed', 'partial', 'indeterminate')
           for r in (*baseline.values(), *candidate.values())):
        return False
    return all(not (r['counts']['FP'] or r['counts']['FN'] or r['contract_failures']
                    or r.get('detection_counts', {}).get('FP') or r.get('detection_counts', {}).get('FN')) for r in candidate.values())
