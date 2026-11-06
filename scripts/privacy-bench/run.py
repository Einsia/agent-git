#!/usr/bin/env python3
"""Download selected binaries, evaluate a fixed corpus, and always leave an honest report."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import signal
import subprocess
import sys
import tempfile
import time

from adapter import ADAPTER_VERSION, Worker, cleanup, dumps, version, telemetry
from evaluate import compare, score, succeeds, leaves
from report import render, write_json
from sources import BenchError, Sources

ROOT = Path(__file__).resolve().parents[2]
RUNNER_VERSION = 1


class Cancelled(BenchError):
    pass


def dataset(profile):
    if profile not in ('default', 'entropy', 'all'):
        raise BenchError('Unknown fixed profile')
    root = ROOT / 'benchmarks/privacy'
    manifest = json.loads((root / 'manifest.json').read_text())
    for relative, expected in manifest['files'].items():
        if hashlib.sha256((root / relative).read_bytes()).hexdigest() != expected:
            raise BenchError('Dataset manifest hash mismatch: ' + relative)
    expected = hashlib.sha256(json.dumps(manifest['files'], sort_keys=True).encode()).hexdigest()
    if expected != manifest['dataset_sha256']:
        raise BenchError('Dataset aggregate hash mismatch')
    cases = json.loads((root / 'cases/synthetic.json').read_text())
    annotations = json.loads((root / 'annotations/synthetic.json').read_text())
    selected = json.loads((root / f'profiles/{profile}.json').read_text())['case_ids']
    cases = [c for c in cases if c['id'] in selected]
    if not cases or len(cases) != len(selected) or len({c['id'] for c in cases}) != len(cases):
        raise BenchError('Profile must select unique existing cases')
    for case in cases:
        checks = annotations[case['id']]
        if not checks:
            raise BenchError('Case has no independent expectations: ' + case['id'])
        if case['layer'] == 'secret':
            locations = {p: v for p, v in leaves(case['input']) if isinstance(v, str)}
            if any(c['pointer'] not in locations for c in checks):
                raise BenchError('Annotation pointer does not identify a string')
            for path, value in locations.items():
                cursor = 0
                for check in sorted((c for c in checks if c['pointer'] == path), key=lambda c: c['start']):
                    if check['expect'] not in ('protect', 'keep') or check['start'] != cursor or not cursor < check['end'] <= len(value):
                        raise BenchError('Secret annotations must partition each input string exactly')
                    cursor = check['end']
                if cursor != len(value):
                    raise BenchError('Unannotated input string range')
    return manifest, cases, {c['id']: annotations[c['id']] for c in cases}


def execute(binary, cases, annotations, work, diagnostics, checkpoint):
    results = {c['id']: {'status': 'failed', 'reason': 'not executed'} for c in cases}
    capabilities = {}
    probe = telemetry(binary, work / 'telemetry-probe', {'operation': 'capabilities'})
    capabilities['detector_telemetry'] = probe is not None and 'scan_text' in probe.get('capabilities', [])
    capabilities['memory_policy'] = probe is not None and 'memory_policy' in probe.get('capabilities', [])
    for mode, probe in [('secret', 'protect_text'), ('policy', 'project_publication')]:
        worker = Worker(binary, work / ('probe-' + mode))
        try:
            text = 'offline probe' if mode == 'secret' else dumps({'branch': None, 'log': '', 'view': '',
                   'metadata': {'session': 'agit-' + 'a' * 40, 'runtime': 'claude-code', 'cwd': '/bench'}, 'additional': []})
            response = worker.request(probe, text)
            capabilities[mode] = response['status'] == 'complete'
        except Cancelled:
            raise
        except (BenchError, OSError, ValueError):
            capabilities[mode] = False
        finally:
            worker.close()
    for case in cases:
        name = case['id']
        started = time.monotonic()
        worker = None
        try:
            if case.get('mode') == 'memory':
                if capabilities['memory_policy']:
                    answer = telemetry(binary, work / (name + '-memory'), {
                        'operation': 'memory_policy', 'policy': case['policy'], 'paths': case['paths']})
                    if answer is None:
                        raise BenchError('Advertised memory policy capability failed')
                    outcome = {'status': answer['status'], 'output': answer}
                else:
                    outcome = {'status': 'unsupported', 'reason': 'memory_policy requires the versioned offline entry'}
            elif not capabilities[case['layer']]:
                outcome = {'status': 'failed', 'reason': 'Required worker mode did not pass the capability probe; support could not be established'}
            else:
                worker = Worker(binary, work / name, case.get('entropy', False), case.get('policy'))
                worker.prepare(case)
                initialized = time.monotonic()
                outcome = worker.run(case, prepared=True)
                if (capabilities['detector_telemetry'] and case['layer'] == 'secret'
                        and isinstance(case['input'], str) and not case.get('context')):
                    detection = telemetry(binary, work / (name + '-scan'), {
                        'operation': 'scan_text', 'text': case['input'], 'blocks': case.get('blocks', []),
                        'allows': case.get('allows', []), 'entropy': case.get('entropy', False)})
                    if detection is None:
                        raise BenchError('Advertised detector telemetry failed')
                    outcome['detection'] = detection
                    if detection['status'] != 'complete':
                        outcome['status'] = 'partial'
                if worker.repo:
                    outcome = json.loads(dumps(outcome).replace(str(worker.repo), '{workspace}'))
                outcome['initialization_seconds'] = initialized - started
                outcome['evaluation_seconds'] = time.monotonic() - initialized
        except Cancelled:
            raise
        except (BenchError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as e:
            outcome = {'status': 'failed', 'reason': str(e) if isinstance(e, BenchError) else type(e).__name__}
        finally:
            if worker:
                worker.close()
                write_json(diagnostics / (name + '.json'), worker.raw)
        results[name] = score(case, annotations[name], outcome)
        results[name]['elapsed_seconds'] = time.monotonic() - started
        write_json(checkpoint, results)
    return results, capabilities


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', default=os.getenv('BENCH_BASELINE', 'release:latest'))
    parser.add_argument('--candidate', default=os.getenv('BENCH_CANDIDATE', 'dev:latest'))
    parser.add_argument('--profile', choices=('default', 'entropy', 'all'), default=os.getenv('BENCH_PROFILE', 'default'))
    parser.add_argument('--output', type=Path, default=Path('privacy-bench-report'))
    parser.add_argument('--use-glab-token', action='store_true', help='Explicit local verification with the configured read_api identity')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        parser.error('Output directory must be empty to prevent mixing runs')
    (output / 'diagnostics').mkdir()
    if hasattr(os, 'sched_setaffinity'):
        os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
    start = time.monotonic()
    result = {'schema_version': 1, 'status': 'execution_failed', 'profile': args.profile,
              'cases': [], 'annotations': {}, 'baseline': {}, 'candidate': {}}
    manifest = {}
    lock = {'schema_version': 1, 'runner_version': RUNNER_VERSION, 'adapter_version': ADAPTER_VERSION,
            'runner': {'system': platform.system(), 'machine': platform.machine(), 'python': platform.python_version()},
            'selectors': {'baseline': args.baseline, 'candidate': args.candidate},
            'runner_files_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                    for p in sorted(Path(__file__).parent.glob('*.py'))}}
    exit_code = 2
    def interrupted(signum, _frame):
        raise Cancelled(f'Run interrupted by signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        manifest, cases, annotations = dataset(args.profile)
        result.update(cases=cases, annotations=annotations)
        for role in ('baseline', 'candidate'):
            result[role] = {c['id']: score(c, annotations[c['id']], {'status': 'failed', 'reason': 'not executed'})
                            for c in cases}
        lock.update(dataset_version=manifest['dataset_version'], dataset_sha256=manifest['dataset_sha256'],
                    annotation_version=manifest['annotation_version'])
        lock['runner_commit'] = subprocess.check_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'], text=True).strip()
        render(output, result, lock, manifest)
        token = os.environ.pop('BENCH_GITLAB_READ_TOKEN', None)
        if args.use_glab_token:
            if os.getenv('CI'):
                raise BenchError('CI must use its dedicated BENCH_GITLAB_READ_TOKEN')
            response = subprocess.run(['glab', 'config', 'get', 'token', '--host', 'git.xiaoaojianghu.fun:114'],
                                      capture_output=True, text=True, timeout=15)
            if response.returncode:
                raise BenchError('Cannot read the explicitly selected local glab credential')
            token = response.stdout.strip()
        sources = Sources(token)
        sources.preflight()
        with tempfile.TemporaryDirectory(prefix='privacy-bench-') as temporary:
            work = Path(temporary)
            binaries = {}
            for role, selector in [('baseline', args.baseline), ('candidate', args.candidate)]:
                binaries[role], lock[role] = sources.resolve(selector, work / role / 'binary')
                write_json(output / 'versions.lock.json', lock)
            del sources, token
            result['timing'] = {'download_seconds': time.monotonic() - start}
            if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'AMD64'):
                raise BenchError('Verified Linux x64 artifacts; execution requires a Linux x64 runner')
            for role in ('baseline', 'candidate'):
                actual = version(binaries[role], work / role / 'version')
                if actual != lock[role]['version']:
                    raise BenchError(f'{role} --version differs from its verified artifact')
                lock[role]['verified_cli_version'] = actual
            write_json(output / 'versions.lock.json', lock)
            result['capabilities'] = {}
            for role in ('baseline', 'candidate'):
                diagnostics = output / 'diagnostics' / role
                diagnostics.mkdir()
                result[role], result['capabilities'][role] = execute(
                    binaries[role], cases, annotations, work / role / 'cases', diagnostics, diagnostics / 'progress.json')
                render(output, result, lock, manifest)
            result['comparison'] = compare(cases, result['baseline'], result['candidate'])
            success = succeeds(cases, result['baseline'], result['candidate'])
            result['status'] = 'passed' if success else 'failed'
            exit_code = 0 if success else 1
    except Exception as e:
        result['error'] = str(e) if isinstance(e, BenchError) else f'Execution failed ({type(e).__name__})'
    finally:
        cleanup()
        for role in ('baseline', 'candidate'):
            checkpoint = output / 'diagnostics' / role / 'progress.json'
            if checkpoint.exists():
                result[role] = json.loads(checkpoint.read_text())
        if result['baseline'] and result['candidate']:
            result['comparison'] = compare(result['cases'], result['baseline'], result['candidate'])
        result.setdefault('timing', {})['total_seconds'] = time.monotonic() - start
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        result['resources'] = {'child_user_cpu_seconds': usage.ru_utime, 'child_system_cpu_seconds': usage.ru_stime,
                               'child_maxrss_native_units': usage.ru_maxrss,
                               'maxrss_unit': 'KiB on Linux; bytes on macOS',
                               'scope': 'all terminated children for this runner process, including probes'}
        report_start = time.monotonic()
        render(output, result, lock, manifest)
        result['timing']['report_seconds'] = time.monotonic() - report_start
        render(output, result, lock, manifest)
    print(f'Privacy benchmark: {result["status"]}; artifacts: {output}/report.html and results.json')
    if 'comparison' in result:
        for role, summary in result['comparison']['all'].items():
            print(f'{role}: completion={summary["completion"]}')
            for layer in ('secret', 'policy'):
                counts = summary[layer]
                print(f'  {layer}: TP={counts["TP"]} FP={counts["FP"]} FN={counts["FN"]} TN={counts["TN"]}; contract failures={counts["contract_failures"]}')
    if 'error' in result:
        print(result['error'])
    return exit_code


if __name__ == '__main__':
    sys.exit(main())
