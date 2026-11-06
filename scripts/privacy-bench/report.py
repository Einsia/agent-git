"""A self-contained, escaped report keeps raw evidence separate from normalized comparisons."""
import difflib
import html
import json



def pretty(value):
    return json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True)


def normalized(result, case):
    text = pretty(result.get('output'))
    originals = sorted(set(result.get('mappings', {}).values()), key=lambda value: pretty(case['input']).find(value))
    for token, original in result.get('mappings', {}).items():
        text = text.replace(token, f'<protected-value-{originals.index(original) + 1}>')
    return text


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(pretty(value) + '\n')
    temporary.replace(path)


def assessment(result):
    if result['status'] != 'complete':
        return result['status']
    counts = result.get('counts', {})
    return 'failed' if counts.get('FP') or counts.get('FN') or result.get('contract_failures') else 'passed'


def render(directory, result, lock, manifest):
    directory.mkdir(parents=True, exist_ok=True)
    write_json(directory / 'results.json', result)
    write_json(directory / 'versions.lock.json', lock)
    write_json(directory / 'dataset-manifest.json', manifest)
    esc = html.escape
    sections = []
    tables = []
    comparison = result.get('comparison')
    if comparison:
        for scope in ('all', 'common'):
            rows = []
            for role, summary in comparison[scope].items():
                for layer in ('secret', 'policy'):
                    metrics = summary[layer]
                    rate = lambda key: 'N/A' if metrics[key] is None else f'{metrics[key]:.1%}'
                    rows.append('<tr>' + ''.join(f'<td>{esc(str(value))}</td>' for value in (
                        role, layer, metrics['TP'], metrics['FP'], metrics['FN'], metrics['TN'],
                        rate('precision'), rate('recall'), rate('false_negative_rate'),
                        rate('normal_case_fp_rate') if layer == 'secret' else 'N/A', metrics['contract_failures'])) + '</tr>')
            columns = ('Version', 'Layer', 'TP', 'FP', 'FN', 'TN', 'Precision', 'Recall', 'FN rate', 'Normal-case FP rate', 'Contract failures')
            tables.append(f'<h3>{esc(scope.title())} completed cases</h3><table><tr>' +
                          ''.join(f'<th>{esc(c)}</th>' for c in columns) + '</tr>' + ''.join(rows) + '</table>')
        for scope in ('all', 'common'):
            rows = []
            for role, metrics in comparison['detection'][scope].items():
                rows.append('<tr>' + ''.join(f'<td>{esc(str(value))}</td>' for value in (
                    role, metrics['completed_cases'], metrics['TP'], metrics['FP'], metrics['FN'], metrics['TN'],
                    json.dumps(metrics['completion'], sort_keys=True))) + '</tr>')
            columns = ('Version', 'Completed', 'TP', 'FP', 'FN', 'TN', 'Detector states')
            tables.append(f'<h3>Detection: {esc(scope)} set</h3><table><tr>' +
                          ''.join(f'<th>{esc(c)}</th>' for c in columns) + '</tr>' + ''.join(rows) + '</table>')

    for case in result.get('cases', []):
        name = case['id']
        old = result['baseline'].get(name, {'status': 'failed'})
        new = result['candidate'].get(name, {'status': 'failed'})
        difference = difflib.HtmlDiff(wrapcolumn=95).make_table(
            normalized(old, case).splitlines(), normalized(new, case).splitlines(),
            'Baseline', 'Candidate', context=False)
        sections.append(f'<details><summary>{esc(name)}: protection {esc(assessment(old))} → {esc(assessment(new))}; '
                        f'detection {esc(old.get("detection_status", "not_executed"))} → '
                        f'{esc(new.get("detection_status", "not_executed"))}</summary>'
                        f'<p>{esc(case["description"])}</p><h3>Input and configuration</h3><pre>{esc(pretty(case))}</pre>'
                        f'<h3>Independent annotations</h3><pre>{esc(pretty(result["annotations"][name]))}</pre>'
                        f'<h3>Output difference</h3>{difference}<h3>Baseline evidence</h3><pre>{esc(pretty(old))}</pre>'
                        f'<h3>Candidate evidence</h3><pre>{esc(pretty(new))}</pre></details>')
    page = ('<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width">'
            '<title>Privacy benchmark</title><style>body{font:15px system-ui;margin:2rem;color:#17202a}'
            'pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f3f5f7;padding:1rem}'
            'table{border-collapse:collapse}td,th{border:1px solid #ddd;padding:6px;text-align:left}'
            'details{border:1px solid #ccc;padding:1rem;margin:1rem 0}summary{cursor:pointer;font-weight:bold}'
            '.diff{font:12px monospace;border-collapse:collapse;max-width:100%;display:block;overflow:auto}'
            '.diff td{padding:3px}.diff_header{background:#eee}.diff_add{background:#d5f5e3}'
            '.diff_sub{background:#fadbd8}.diff_chg{background:#fcf3cf}</style><h1>Privacy benchmark</h1>'
            f'<p>Result: <strong>{esc(result["status"])}</strong></p>'
            '<p>Secret counts use annotated ranges. Policy counts use annotated target nodes. '
            'They are not added together. Null metrics mean N/A. Noncompleted cases are never TN. '
            'Protection and detection have independent completion states and common comparison sets. '
            'Unsupported or inapplicable detection is distinct from a failed or partial detector run. '
            'Complete means execution completed, not that its quality checks passed. Recovery-aligned output '
            'ranges are not detector findings. GNU/musl and build differences affect performance.</p>'
            f'<h2>Versions and dataset</h2><pre>{esc(pretty(lock))}</pre>'
            f'<h2>Coverage, quality and changes</h2>{"".join(tables)}<pre>{esc(pretty(result.get("comparison")))}</pre>'
            f'<h2>Execution diagnostics</h2><pre>{esc(pretty({k:v for k,v in result.items() if k in ("error", "timing", "resources", "capabilities", "profile")}))}</pre>'
            '<p>Per-request raw responses are in diagnostics/. All inputs and recovery dictionaries are synthetic.</p>'
            + ''.join(sections) + '</html>')
    (directory / 'report.html').write_text(page)
