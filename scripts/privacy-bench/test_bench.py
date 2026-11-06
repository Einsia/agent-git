"""Contracts that prevent a broken benchmark from reporting an apparent quality improvement."""
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
import urllib.error
from unittest.mock import patch

from adapter import Worker, clean_env
from evaluate import compare, score, summarize, succeeds
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
        checks = [{'pointer': '/body', 'expect': 'hide', 'value': 'private'},
                  {'pointer': '/body', 'expect': 'keep', 'value': 'public'},
                  {'pointer': '/body', 'expect': 'rewrite', 'value': 'old', 'replacement': 'new'}]
        result = score(case, checks, {'status': 'complete', 'output': {'body': 'private old'},
                                     'contracts': {'paired_call': False}})
        self.assertEqual(result['counts']['FN'], 1)
        self.assertEqual(result['counts']['FP'], 1)
        self.assertEqual(result['contract_failures'], ['paired_call', 'rewrite:2'])
        memory = score(case, [{'pointer': '/action', 'expect': 'exclude', 'value': 'excluded'}],
                       {'status': 'complete', 'output': {'action': 'allowed'}})
        self.assertEqual(memory['counts']['FN'], 1)
        allowed = score(case, [{'pointer': '/action', 'expect': 'allow', 'value': 'allowed'}],
                        {'status': 'complete', 'output': {'action': 'excluded'}})
        self.assertEqual(allowed['counts']['FP'], 1)

    def test_old_baseline_defects_do_not_block_a_correct_candidate(self):
        case = {'id': 'secret', 'layer': 'secret', 'input': 'SECRET'}
        checks = [{'pointer': '', 'start': 0, 'end': 6, 'expect': 'protect'}]
        old = score(case, checks, {'status': 'complete', 'output': 'SECRET'})
        new = score(case, checks, {'status': 'complete', 'output': PLACEHOLDER, 'mappings': {PLACEHOLDER: 'SECRET'}})
        self.assertTrue(succeeds([case], {'secret': old}, {'secret': new}))
        self.assertEqual(compare([case], {'secret': old}, {'secret': new})['changes']['fixed_FN'],
                         [{'case': 'secret', 'annotation': 0}])


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
