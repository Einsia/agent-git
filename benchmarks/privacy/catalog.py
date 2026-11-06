"""Render the reviewed purpose of every case without consulting any CLI verdict."""


def render_catalog(cases):
    lines = [
        '# Privacy benchmark case catalog', '',
        'This catalog covers every checked-in case. Inputs and annotations are in',
        '[the corpus](../benchmarks/privacy/cases/synthetic.json) and',
        '[the annotations](../benchmarks/privacy/annotations/synthetic.json).', '',
        'Scenario expectations are selected from credential semantics, explicit user rules,',
        'publication contracts and public format specifications before a tested CLI runs.',
        'A detector miss does not authorize weakening an expectation. A malformed fixture',
        'is corrected against its format specification, with the reason documented separately.', '',
        'All values are synthetic and endpoints use reserved example domains. Provider-shaped',
        'values are not issued by a service. The signing key pair was generated solely for',
        'this fixture, is public test material, and must not secure any deployment.',
        'The JWT has a verifiable test signature; the PNG has valid chunks, checksums and pixels.', '',
        'Run `python3 benchmarks/privacy/validate_fixtures.py --external` to check declared',
        'Python, JavaScript, shell, INI, JSON, HTTP, URI, JWT, PEM and PNG specimens. The',
        'external checks require Node.js, OpenSSL and a POSIX shell and only parse or inspect',
        'the fixtures. Without `--external`, required external checks are listed as deferred.',
        'These checks are independent of the privacy scanner and never contact a credential',
        'issuer. Workflow validation replays the fixed calculator edit and runs its local',
        'check script in an isolated temporary project; it does not execute recorded shell',
        'commands or start an agent. Native transcript fields and linkage are validated',
        'by the benchmark input contracts. Natural-language prose has no formal syntax check.', '',
        'Existing boundary probes remain useful alongside the realistic scenario fixtures.',
        'In particular, `private-key` is a synthetic region-recognition probe rather than a',
        'complete PEM key, and the original `attachment-*` probes use encoded stand-in bytes.',
        'The scenario key-pair and image cases add fully valid artifacts for those workflows.', '',
        'The `native-*` cases are protocol-contract probes with synthetic Read/ReadPublic tools.',
        'The `workflow-*` cases use separately documented tool profiles and a read/edit/test',
        'sequence. They are constructed traces, not recordings of live agent execution.',
        'See [workflow profiles and limits](privacy-benchmark-workflows.md).', '',
        'The `incident-*` cases reconstruct reviewed reports with new synthetic values.',
        'Their evidence notes distinguish directly observed anomalies from workflow',
        'corroboration and report-only facts. See [incident triage](privacy-benchmark-incidents.md).', '',
        'Regenerate this catalog with `python3 benchmarks/privacy/generate.py`.', '',
    ]
    for case in cases:
        name = case['id']
        profiles = ['all']
        if not case.get('entropy') and case.get('mode') != 'memory':
            profiles.insert(0, 'default')
        if case.get('entropy') or name == 'bare-entropy-default':
            profiles.insert(0, 'entropy')
        scenario = case.get('scenario', 'A focused synthetic boundary probe for the behavior described below.')
        contract = case['description']
        if name.startswith('native-'):
            scenario = f"A {case['runtime']} protocol probe uses synthetic Read/ReadPublic tools to isolate envelope routing and call/result association; it does not represent that agent's default toolset."
            contract = ('Keep both tools and their results in native format, including event kinds, roles and IDs; public-view omissions do not apply to plain capture.'
                        if case['mode'] == 'plain' else
                        'Omit only the tool selected by the runtime-specific override; preserve the other tool, its result and every call/result association.')
        mode = case.get('mode', 'entropy opt-in' if case.get('entropy') else 'default detection')
        lines.extend([f'## {name}', '',
                      f"- **Coverage kind:** {case.get('coverage_kind', 'workflow scenario' if name.startswith('scenario-') else 'boundary probe')}.",
                      f"- **Scope:** {case['layer']}; {mode}; runtime: {case.get('runtime', 'not runtime-specific')}; profiles: {', '.join(profiles)}.",
                      f'- **Scenario:** {scenario}', f'- **Expected behavior:** {contract}'])
        basis = case.get('expectation_basis', 'docs/privacy-benchmark.md; docs/privacy-plain-publication.md; docs/secret-context-classification.md')
        links = []
        for source in basis.split('; '):
            links.append(f'[{source.rstrip("/").split("/")[-1]}]({source if source.startswith("https:") else source.removeprefix("docs/")})')
        lines.append('- **Expectation basis:** ' + ', '.join(links) + '.')
        if case.get('evidence'):
            evidence = case['evidence']
            links = [f'[CLI issue #{url.rsplit("/", 1)[-1]}]({url})' for url in evidence['issues']]
            if evidence.get('session'):
                links.append(f"[Frozen Hub session]({evidence['session']['url']})")
            lines.append('- **Incident evidence:** ' + ', '.join(links) + '.')
            lines.append('- **Evidence boundary:** ' + evidence['observation'])
        if case.get('tool_profile'):
            lines.append('- **Tool profile:** ' + case['tool_profile'] + '.')
            sources = case['tool_sources'].split('; ')
            links = []
            for source in sources:
                target = source if source.startswith('https:') else '../' + source
                links.append(f'[{source.rstrip("/").split("/")[-1]}]({target})')
            lines.append('- **Tool-contract sources:** ' + ', '.join(links) + '.')
        formats = sorted({v['format'] for v in case.get('validity', [])})
        lines.append('- **Validity checks:** ' + (', '.join(formats) if formats else 'corpus ranges, native layout where applicable, and independent annotation review') + '.')
        lines.append('')
    return '\n'.join(lines)
