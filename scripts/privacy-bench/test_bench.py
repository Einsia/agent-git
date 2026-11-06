"""Contracts that prevent a broken benchmark from reporting an apparent quality improvement."""
import io
import copy
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
import urllib.error
import runpy
from unittest.mock import patch, MagicMock

from adapter import Worker, clean_env
from evaluate import compare, score, summarize, succeeds
from contracts import lookup, MISSING, RUNTIMES, layout_records, policy_record_contracts, record_contracts, validate_case, validate_stream
from report import render
from sources import BenchError, Sources, tar_members

PLACEHOLDER = '‹secret:credential:abcdefghijkl›'


class ScoringContract(unittest.TestCase):
    def test_each_occurrence_partial_protection_and_overprotection_have_distinct_verdicts(self):
        case = {'id': 'two', 'layer': 'secret', 'input': 'safe SECRET + SECRET end'}
        annotations = [
            {'pointer': '', 'start': 0, 'end': 5, 'expect': 'keep'},
            {'pointer': '', 'start': 5, 'end': 11, 'expect': 'protect'},
            {'pointer': '', 'start': 11, 'end': 14, 'expect': 'keep'},
            {'pointer': '', 'start': 14, 'end': 20, 'expect': 'protect'},
            {'pointer': '', 'start': 20, 'end': 24, 'expect': 'keep'},
        ]
        def evaluate(output, value):
            return score(case, annotations, {'status': 'complete', 'output': output,
                                            'mappings': {PLACEHOLDER: value}, 'contracts': {}})
        missed = evaluate('safe ' + PLACEHOLDER + ' + SECRET end', 'SECRET')
        self.assertEqual(missed['counts'], {'TP': 1, 'FP': 0, 'FN': 1, 'TN': 3})
        partial = evaluate('safe ' + PLACEHOLDER + 'RET + SECRET end', 'SEC')
        self.assertEqual(partial['counts']['FN'], 2)
        over = evaluate(PLACEHOLDER + ' + SECRET end', 'safe SECRET')
        self.assertEqual(over['counts']['FP'], 1)
        self.assertEqual(over['counts']['TP'], 1)
        empty = evaluate('', '')
        self.assertEqual(empty['status'], 'indeterminate')
        self.assertEqual(sum(empty['counts'].values()), 0)

    def test_coverage_loss_cannot_improve_common_set_metrics_or_pass(self):
        case = {'id': 'normal', 'layer': 'secret', 'input': 'normal'}
        checks = [{'pointer': '', 'start': 0, 'end': 6, 'expect': 'keep'}]
        complete = score(case, checks, {'status': 'complete', 'output': 'normal'})
        for status in ('failed', 'partial', 'unsupported', 'indeterminate'):
            failed = score(case, checks, {'status': status})
            summary = summarize([case], {'normal': failed})
            self.assertEqual(summary['secret']['TN'], 0)
            self.assertIsNone(summary['secret']['recall'])
            comparison = compare([case], {'normal': complete}, {'normal': failed})
            self.assertEqual(comparison['common_completed_ids'], [])
            self.assertEqual(comparison['changes']['lost_coverage'], ['normal'])
            self.assertFalse(succeeds([case], {'normal': complete}, {'normal': failed}))

    def test_policy_false_hiding_missing_hiding_and_wrong_rewrite_are_separate(self):
        case = {'id': 'policy', 'layer': 'policy'}
        checks = [{'pointer': '/private', 'expect': 'hide', 'value': 'private', 'replacement': '[omitted]'},
                  {'pointer': '/public', 'expect': 'keep', 'value': 'public'},
                  {'pointer': '/body', 'expect': 'rewrite', 'value': 'safe old tail', 'replacement': 'safe new tail'}]
        result = score(case, checks, {'status': 'complete', 'output': {'private': 'private', 'public': '', 'body': 'new'},
                                     'contracts': {'paired_call': False}})
        self.assertEqual(result['counts']['FN'], 1)
        self.assertEqual(result['counts']['FP'], 1)
        self.assertEqual(result['contract_failures'], ['paired_call', 'hide_replacement:0', 'rewrite:2'])
        for output in ({}, {'private': None}, {'private': ''}):
            failed = score(case, checks[:1], {'status': 'complete', 'output': output})
            self.assertTrue(failed['contract_failures'])
            self.assertFalse(succeeds([case], {'policy': failed}, {'policy': failed}))
        removed = [{'pointer': '/body/private', 'expect': 'remove', 'value': 'private'}]
        for output, expected in (({'body': {}}, 'TP'), ({}, 'FN'), ({'body': {'private': None}}, 'FN')):
            result = score(case, removed, {'status': 'complete', 'output': output})
            self.assertEqual(result['checks'][0]['verdict'], expected)
        memory = score(case, [{'pointer': '/action', 'expect': 'exclude', 'value': 'excluded'}],
                       {'status': 'complete', 'output': {'action': 'allowed'}})
        self.assertEqual(memory['counts']['FN'], 1)
        allowed = score(case, [{'pointer': '/action', 'expect': 'allow', 'value': 'allowed'}],
                        {'status': 'complete', 'output': {'action': 'excluded'}})
        self.assertEqual(allowed['counts']['FP'], 1)

    def test_container_changes_cannot_disappear_from_leaf_comparison(self):
        case = {'id': 'structure', 'layer': 'secret', 'input': {'text': 'ok', 'empty': [], 'flag': True}}
        checks = [{'pointer': '/text', 'start': 0, 'end': 2, 'expect': 'keep'}]
        for change in ({'empty': {}}, {'flag': 1}, {'extra': []}):
            result = score(case, checks, {'status': 'complete', 'output': {**case['input'], **change}})
            self.assertEqual(result['status'], 'indeterminate')
            self.assertIn('json_structure', result['contract_failures'])
            self.assertEqual(summarize([case], {'structure': result})['secret']['contract_failures'], 1)

    def test_detector_completion_and_comparison_do_not_depend_on_protection_completion(self):
        case = {'id': 'stages', 'layer': 'secret', 'input': '\u00e9 SECRET'}
        checks = [{'pointer': '', 'start': 0, 'end': 2, 'expect': 'keep'},
                  {'pointer': '', 'start': 2, 'end': 8, 'expect': 'protect'}]
        detection = {'schema_version': 1, 'status': 'complete', 'offset_unit': 'utf8_bytes',
                     'findings': [{'start': 3, 'end': 9}]}
        protected = {'status': 'complete', 'output': '\u00e9 ' + PLACEHOLDER,
                     'mappings': {PLACEHOLDER: 'SECRET'}, 'detection': detection}
        good = score(case, checks, protected)
        self.assertEqual(good['detection_counts']['TP'], 1)
        for bad in ({**detection, 'status': 'partial'}, {**detection, 'findings': [{'start': 1, 'end': 9}]},
                    {**detection, 'offset_unit': 'characters'}):
            result = score(case, checks, {**protected, 'detection': bad})
            self.assertEqual(result['status'], 'complete')
            self.assertEqual(result['counts']['TP'], 1)
            self.assertNotIn('detection_counts', result)
            comparison = compare([case], {'stages': good}, {'stages': result})
            self.assertEqual(comparison['common_completed_ids'], ['stages'])
            self.assertEqual(comparison['detection']['common_completed_ids'], [])
            self.assertFalse(succeeds([case], {'stages': good}, {'stages': result}))
        failed = score(case, checks, {'status': 'failed', 'detection': detection})
        self.assertEqual(failed['detection_counts']['TP'], 1)
        comparison = compare([case], {'stages': good}, {'stages': failed})
        self.assertEqual(comparison['common_completed_ids'], [])
        self.assertEqual(comparison['detection']['common_completed_ids'], ['stages'])

    def test_old_baseline_defects_do_not_block_a_correct_candidate(self):
        case = {'id': 'secret', 'layer': 'secret', 'input': 'SECRET'}
        checks = [{'pointer': '', 'start': 0, 'end': 6, 'expect': 'protect'}]
        old = score(case, checks, {'status': 'complete', 'output': 'SECRET'})
        new = score(case, checks, {'status': 'complete', 'output': PLACEHOLDER, 'mappings': {PLACEHOLDER: 'SECRET'}})
        self.assertTrue(succeeds([case], {'secret': old}, {'secret': new}))
        self.assertEqual(compare([case], {'secret': old}, {'secret': new})['changes']['fixed_FN'],
                         [{'case': 'secret', 'annotation': 0}])


class InputAndProtocolContract(unittest.TestCase):
    def test_advertised_detector_failure_keeps_completed_worker_evidence(self):
        from run import execute
        case = {'id': 'independent-stages', 'layer': 'secret', 'input': 'safe',
                'description': 'Completed protection remains visible when detector execution fails.'}
        checks = [{'pointer': '', 'start': 0, 'end': 4, 'expect': 'keep'}]
        worker = MagicMock()
        worker.repo = None
        worker.raw = []
        worker.request.return_value = {'status': 'complete'}
        worker.run.return_value = {'status': 'complete', 'output': 'safe'}
        def telemetry(_binary, _root, request):
            if request['operation'] == 'capabilities':
                return {'schema_version': 1, 'capabilities': ['scan_text']}
            raise BenchError('Advertised detector returned invalid JSON')
        with tempfile.TemporaryDirectory() as directory, patch('run.Worker', return_value=worker), \
                patch('run.telemetry', side_effect=telemetry):
            root = Path(directory)
            results, _ = execute(root / 'binary', [case], {case['id']: checks}, root / 'work', root, root / 'progress.json')
            result = results[case['id']]
            self.assertEqual(result['status'], 'complete')
            self.assertEqual(result['counts']['TN'], 1)
            self.assertEqual(result['detection_status'], 'failed')
            self.assertFalse(succeeds([case], results, results))
            comparison = compare([case], results, results)
            render(root, {'status': 'failed', 'cases': [case], 'annotations': {case['id']: checks},
                          'baseline': results, 'candidate': results, 'comparison': comparison}, {}, {})
            page = (root / 'report.html').read_text()
            self.assertIn('Detection: common set', page)
            self.assertIn('protection passed', page)
            self.assertIn('detection failed', page)

    def test_generated_annotations_select_occurrences_and_reject_invalid_expectations(self):
        from run import dataset
        root = Path(__file__).resolve().parents[2] / 'benchmarks/privacy'
        generated = runpy.run_path(str(root / 'generate.py'))
        manifest, cases, annotations = dataset('all')
        self.assertEqual(cases, generated['CASES'])
        self.assertEqual(annotations, generated['ANNOTATIONS'])
        self.assertEqual(manifest['annotation_version'], 3)
        catalog = runpy.run_path(str(root / 'catalog.py'))['render_catalog'](cases)
        self.assertEqual((root.parents[1] / 'docs/privacy-benchmark-cases.md').read_text(), catalog)
        runpy.run_path(str(root / 'validate_fixtures.py'))['validate'](cases, root)
        secret = generated['secret']
        secret('selected', 'same same', {'': [{'value': 'same', 'count': 2, 'occurrences': [1]}]}, 'Occurrence contract')
        self.assertEqual(generated['ANNOTATIONS']['selected'], [
            {'pointer': '', 'start': 0, 'end': 5, 'expect': 'keep'},
            {'pointer': '', 'start': 5, 'end': 9, 'expect': 'protect'}])
        for protected in ({'/missing': ['same']}, {'': ['absent']}, {'': ['']},
                          {'': [{'value': 'same', 'count': 1}]}, {'': ['same', 'same']},
                          {'': [{'value': 'same', 'start': 1, 'end': 5}]}):
            with self.subTest(protected=protected), self.assertRaises(ValueError):
                secret('invalid', 'same same', protected, 'Invalid annotation')
        case = {'id': 'policy-input', 'layer': 'policy', 'mode': 'encrypted_public', 'runtime': 'claude-code',
                'record_layout': [], 'input': {'records': [], 'body': 'safe old tail'}}
        check = {'pointer': '/body', 'input_pointer': '/body', 'expect': 'rewrite',
                 'value': 'safe old tail', 'replacement': 'safe new tail'}
        validate_case(case, [check])
        validate_case({**case, 'branch': 'release', 'workspace_files': {'src/internal/checklist.md': 'review'}}, [check])
        for change in ({'branch': ''}, {'branch': True}, {'workspace_files': {'../outside': 'bad'}},
                       {'workspace_files': {'.git/config': 'bad'}}, {'workspace_files': {'C:/outside': 'bad'}}):
            with self.subTest(change=change), self.assertRaises(BenchError):
                validate_case({**case, **change}, [check])
        for change in ({'expect': 'rewirte'}, {'value': 'absent'}, {'input_pointer': '/absent'},
                       {'pointer': '/bad~escape'}, {'typo': True}):
            with self.subTest(change=change), self.assertRaises(BenchError):
                validate_case(case, [{**check, **change}])
        self.assertIs(lookup({}, '/missing'), MISSING)
        self.assertIsNone(lookup({'missing': None}, '/missing'))

    def test_policy_runtime_and_native_records_reach_the_worker_without_reencoding_the_schema(self):
        from run import dataset
        _, cases, annotations = dataset('all')
        native = [c for c in cases if c['id'].startswith(('native-', 'workflow-'))]
        self.assertEqual({c['runtime'] for c in native}, set(RUNTIMES))
        for case in native:
            case = {**case, 'branch': 'release'}
            with self.subTest(case=case['id']):
                worker = Worker.__new__(Worker)
                worker.repo = None
                def request(mode, text):
                    if case['mode'] == 'plain':
                        self.assertEqual(mode, 'protect_envelopes')
                        log = text
                        response = text
                    else:
                        self.assertEqual(mode, 'project_publication')
                        publication = json.loads(text)
                        self.assertEqual(publication['metadata']['runtime'], case['runtime'])
                        self.assertEqual(publication['branch'], 'release')
                        log = publication['log']
                        projected = layout_records(case, case['input'])
                        for record in projected:
                            for block in record['message']['content']:
                                if block['type'] == 'tool_result':
                                    block['is_error'] = False
                        projected_log = ''.join(json.dumps({'content': record}) + '\n' for record in projected)
                        response = json.dumps({'log': projected_log, 'view': projected_log, 'metadata': {}, 'report': {}})
                    envelopes = [json.loads(line) for line in log.splitlines()]
                    self.assertTrue(all(e['_source'] == case['runtime'] for e in envelopes))
                    self.assertEqual([e['content'] for e in envelopes], case['input']['records'])
                    return {'status': 'complete', 'content': response}
                with patch.object(worker, 'request', side_effect=request):
                    result = worker.run(case, prepared=True)
                self.assertTrue(all(result['contracts'].values()))
                if case['mode'] == 'plain':
                    changed = copy.deepcopy(case['input'])
                    path = case['record_layout'][0]['preserve'][0]
                    parent, field = path.rsplit('/', 1)
                    lookup(changed, parent)[field] = 'changed-role-or-kind'
                    self.assertFalse(policy_record_contracts(case, case['input'], changed)['native_protocol_fields'])
        case = next(c for c in native if c['runtime'] == 'codex')
        for mutation in ({'runtime': 'unknown'}, {'runtime': None},
                         {'input': {**case['input'], 'metadata': {'runtime': 'claude-code'}}},
                         {'record_layout': []}):
            with self.subTest(mutation=mutation), self.assertRaises(BenchError):
                validate_case({**case, **mutation}, annotations[case['id']])

    def test_workflow_edit_must_match_the_read_file_and_freeform_input_stays_text(self):
        from run import dataset
        root = Path(__file__).resolve().parents[2] / 'benchmarks/privacy'
        validate = runpy.run_path(str(root / 'validate_fixtures.py'))['validate']
        _, cases, _ = dataset('all')
        case = next(c for c in cases if c['id'] == 'workflow-codex-calculator-plain')
        validate([case], root, external=True)
        path = case['workflow']['steps'][1]['arguments']['pointer']
        parent, field = path.rsplit('/', 1)
        for transform in (lambda patch: patch.replace('-    return value + 1', '-    return value + 9'),
                          lambda patch: {'patch': patch}):
            broken = copy.deepcopy(case)
            at = lookup(broken['input'], parent)
            at[field] = transform(at[field])
            with self.assertRaises(ValueError):
                validate([broken], root)

    def test_completed_native_output_with_unreadable_arguments_retains_scoring_evidence(self):
        from run import dataset
        _, cases, annotations = dataset('all')
        case = next(c for c in cases if c['id'] == 'incident-codex-call-linkage')
        output = copy.deepcopy(case['input'])
        output['records'][0]['payload']['arguments'] = PLACEHOLDER
        contracts = policy_record_contracts(case, case['input'], output)
        self.assertFalse(contracts['encoded_native_fields'])
        result = score(case, annotations[case['id']], {'status': 'complete', 'output': output, 'contracts': contracts})
        self.assertEqual(result['status'], 'complete')
        self.assertIn('encoded_native_fields', result['contract_failures'])
        self.assertEqual(result['checks'][0]['verdict'], 'TN')
        self.assertEqual(result['checks'][2]['verdict'], 'rewrite_fail')
        self.assertFalse(succeeds([case], {case['id']: result}, {case['id']: result}))

    def test_tool_aliases_preserve_each_reference_and_reject_duplicate_or_swapped_results(self):
        generated = runpy.run_path(str(Path(__file__).resolve().parents[2] / 'benchmarks/privacy/generate.py'))
        original = generated['tools']('first', 'FIRST', 'a') + generated['tools']('second', 'SECOND', 'b')
        output = copy.deepcopy(original)
        for index, alias in ((0, 'public-a'), (2, 'public-b')):
            output[index]['message']['content'][0]['id'] = alias
            output[index + 1]['message']['content'][0].update(tool_use_id=alias, is_error=False)
        self.assertTrue(all(record_contracts(original, output).values()))
        self.assertTrue(all(record_contracts(original, original, plain=True).values()))
        for references in (('public-a', 'public-a'), ('public-b', 'public-a')):
            broken = copy.deepcopy(output)
            for index, alias in zip((1, 3), references):
                broken[index]['message']['content'][0]['tool_use_id'] = alias
            self.assertFalse(record_contracts(original, broken)['paired_calls'])
        plain = copy.deepcopy(original)
        plain[3]['message']['content'][0]['tool_use_id'] = 'a'
        self.assertFalse(record_contracts(original, plain, plain=True)['paired_calls'])
        checks = [{'pointer': f'/records/{i}/message/content/0/content', 'expect': 'keep', 'value': body}
                  for i, body in ((1, 'FIRST'), (3, 'SECOND'))]
        swapped = copy.deepcopy(output)
        swapped[1]['message']['content'][0]['content'] = 'SECOND'
        swapped[3]['message']['content'][0]['content'] = 'FIRST'
        result = score({'layer': 'policy'}, checks, {'status': 'complete', 'output': {'records': swapped},
                       'contracts': record_contracts(original, swapped)})
        self.assertEqual(result['counts']['FP'], 2)

    def test_stream_coordinates_are_ordered_code_points_and_consumption_is_utf8_aligned(self):
        case = {'layer': 'secret', 'input': '\u00e9xx', 'context': 'prefix', 'stream_cuts': [1]}
        validate_stream(case)
        for cuts in ([0], [3], [2, 1], [1, 1], [True], [], [1.0]):
            with self.subTest(cuts=cuts), self.assertRaises(ValueError):
                validate_stream({**case, 'stream_cuts': cuts})
        worker = Worker.__new__(Worker)
        worker.repo = None
        with patch.object(worker, 'request', return_value={'status': 'complete', 'content': '', 'consumed': 1}) as request:
            with self.assertRaisesRegex(BenchError, 'UTF-8'):
                worker.run(case, prepared=True)
            request.assert_called_once_with('protect_stream', '\u00e9', 'prefix')


class DistributionContract(unittest.TestCase):
    def test_credentials_are_required_and_redirects_cannot_receive_them(self):
        with self.assertRaises(BenchError):
            Sources('')
        source = Sources('synthetic-noncredential')
        with self.assertRaises(BenchError):
            source.get('https://other.invalid/package', private=True)
        from sources import NoRedirect
        with self.assertRaises(BenchError):
            NoRedirect().redirect_request(None, None, 302, None, None, 'https://other.invalid')
        environment = clean_env(Path('/tmp/synthetic-bench'))
        self.assertNotIn('BENCH_GITLAB_READ_TOKEN', environment)
        self.assertNotIn('CI_JOB_TOKEN', environment)
        self.assertNotIn('SSH_AUTH_SOCK', environment)
        self.assertEqual(environment['AGIT_TELEMETRY_DISABLED'], '1')
        for status in (401, 403):
            error = urllib.error.HTTPError('https://git.xiaoaojianghu.fun:114/api/v4/projects/87/jobs',
                                           status, 'synthetic-noncredential', {}, None)
            with patch.object(source.opener, 'open', side_effect=error):
                with self.assertRaises(BenchError) as caught:
                    source.preflight()
                self.assertEqual(str(caught.exception), f'Distribution request failed: HTTP {status}')

    def test_archive_integrity_links_and_duplicate_members_are_not_executed(self):
        source = Sources('synthetic-noncredential')
        source.stable = ['1.0.0']
        metadata = {'name': '@einsia/agent-git-linux-x64', 'version': '1.0.0',
                    'dist': {'tarball': 'https://registry.npmjs.org/fixture.tgz', 'integrity': 'sha512-invalid'}}
        with patch.object(source, 'npm', return_value=metadata), patch.object(source, 'get', return_value=b'tampered'):
            with self.assertRaisesRegex(BenchError, 'integrity verification failed'):
                source.release('release:1.0.0')
        for linked in (False, True):
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode='w:gz') as archive:
                item = tarfile.TarInfo('agit')
                if linked:
                    item.type = tarfile.SYMTYPE
                    item.linkname = '/etc/passwd'
                    archive.addfile(item)
                else:
                    archive.addfile(item, io.BytesIO())
                    archive.addfile(item, io.BytesIO())
            with self.assertRaises(BenchError):
                tar_members(stream.getvalue(), ('agit',))

    def test_dev_selection_uses_successful_jobs_not_pipeline_status_and_never_falls_back(self):
        source = Sources('synthetic-noncredential')
        job = {'id': 42, 'name': 'dev:linux', 'ref': 'main', 'status': 'success',
               'artifacts_file': {'size': 100}, 'pipeline': {'status': 'failed'}}
        with patch.object(source, 'api', return_value=[job]) as api, patch.object(source, 'get', side_effect=BenchError('HTTP 404')) as get:
            with self.assertRaises(BenchError):
                source.dev('dev:latest')
            self.assertEqual(api.call_count, 1)
            self.assertIn('/jobs/42/artifacts', get.call_args.args[0])
            self.assertEqual(get.call_count, 1)

    def test_previous_release_requires_a_distinct_available_platform_version(self):
        source = Sources('synthetic-noncredential')
        source.stable = ['1.0.0']
        with patch.object(source, 'npm', return_value={'version': '1.0.0'}):
            with self.assertRaises(BenchError):
                source.release('release:previous')


class ReportContract(unittest.TestCase):
    def test_report_escapes_constructed_input_and_retains_failure_evidence(self):
        case = {'id': 'markup', 'layer': 'secret', 'input': '<script>alert(1)</script>', 'description': '<script>'}
        failed = {'status': 'failed', 'reason': 'process ended'}
        result = {'status': 'failed', 'cases': [case], 'annotations': {'markup': []},
                  'baseline': {'markup': failed}, 'candidate': {'markup': failed}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            render(root, result, {}, {})
            page = (root / 'report.html').read_text()
            self.assertNotIn('<script>', page)
            self.assertIn('&lt;script&gt;', page)
            self.assertIn('process ended', page)
            self.assertEqual(json.loads((root / 'results.json').read_text())['status'], 'failed')


@unittest.skipUnless(os.getenv('BENCH_TEST_BINARY'), 'Set BENCH_TEST_BINARY to exercise a locally built real CLI')
class RealCliContract(unittest.TestCase):
    def test_native_sources_select_their_tool_overrides_and_keep_plain_records(self):
        from run import dataset, execute
        _, cases, annotations = dataset('all')
        cases = [c for c in cases if c['id'].startswith('native-')]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            results, _ = execute(Path(os.environ['BENCH_TEST_BINARY']).resolve(), cases, annotations,
                                 root / 'work', root, root / 'progress.json')
            for name, result in results.items():
                with self.subTest(case=name):
                    self.assertEqual(result['status'], 'complete', result.get('reason'))
                    self.assertEqual(result['counts']['FP'] + result['counts']['FN'], 0, result['checks'])
                    self.assertEqual(result['contract_failures'], [])

    def test_stream_uses_initial_context_and_agrees_with_whole_text_protection(self):
        secret = 'c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0'
        suffix = '" and a safe suffix.\n' + 'Public line.\n' * 8
        case = {'id': 'stream-context', 'layer': 'secret', 'input': secret + suffix,
                'context': '\u00e9 password = "', 'stream_cuts': [5, 20, 60]}
        with tempfile.TemporaryDirectory() as directory:
            worker = Worker(Path(os.environ['BENCH_TEST_BINARY']).resolve(), Path(directory) / 'case')
            try:
                result = score(case, [
                    {'pointer': '', 'start': 0, 'end': len(secret), 'expect': 'protect'},
                    {'pointer': '', 'start': len(secret), 'end': len(case['input']), 'expect': 'keep'},
                ], worker.run(case))
                self.assertEqual(result['status'], 'complete')
                self.assertEqual(result['counts'], {'TP': 1, 'FP': 0, 'FN': 0, 'TN': 1})
                self.assertEqual(result['contract_failures'], [])
            finally:
                worker.close()

    def test_worker_preserves_neighbors_and_recovers_each_synthetic_secret(self):
        binary = Path(os.environ['BENCH_TEST_BINARY']).resolve()
        with tempfile.TemporaryDirectory() as directory:
            worker = Worker(binary, Path(directory) / 'case')
            case = {'id': 'secret', 'layer': 'secret', 'input': 'before demo-private after', 'blocks': ['demo-private']}
            try:
                result = worker.run(case)
                scored = score(case, [
                    {'pointer': '', 'start': 0, 'end': 7, 'expect': 'keep'},
                    {'pointer': '', 'start': 7, 'end': 19, 'expect': 'protect'},
                    {'pointer': '', 'start': 19, 'end': 25, 'expect': 'keep'},
                ], result)
                self.assertEqual(scored['status'], 'complete')
                self.assertEqual(scored['counts'], {'TP': 1, 'FP': 0, 'FN': 0, 'TN': 2})
                self.assertEqual(scored['contract_failures'], [])
            finally:
                worker.close()


if __name__ == '__main__':
    unittest.main()
