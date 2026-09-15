"""Real multi-repository, multi-harness tests through the desktop bridge.

Run on the execution machine. Credentials are process-only stdin input when
--gateway is supplied. Evidence stays in a private directory outside checkout.
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


class Client:
    def __init__(self, binary, env, root):
        self.binary, self.env, self.root = binary, env, root
        self.pending, self.frames, self.approvals = {}, [], set()
        self.journal = (root / 'events.jsonl').open('a')

    async def connect(self):
        self.process = await asyncio.create_subprocess_exec(
            self.binary, 'rc', 'local', 'bridge', env=self.env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL, limit=8 * 1024 * 1024)
        self.reader = asyncio.create_task(self.read())

    async def read(self):
        try:
            while line := await self.process.stdout.readline():
                f = json.loads(line)
                if 'id' in f:
                    future = self.pending.pop(f['id'], None)
                    if future and not future.done():
                        if 'error' in f:
                            future.set_exception(RuntimeError(json.dumps(f['error'])))
                        else:
                            future.set_result(f.get('result'))
                else:
                    self.frames.append(f)
                    self.journal.write(json.dumps(f) + '\n')
                    self.journal.flush()
            raise RuntimeError('Bridge closed')
        except (Exception, asyncio.CancelledError) as e:
            for future in self.pending.values():
                if not future.done(): future.set_exception(RuntimeError(str(e)))
            self.pending.clear()

    async def rpc(self, method, **params):
        ident = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        self.process.stdin.write((json.dumps(dict(jsonrpc='2.0', id=ident, method=method, params=params)) + '\n').encode())
        await self.process.stdin.drain()
        return await asyncio.wait_for(future, 90)

    async def disconnect(self):
        if self.process.returncode is None: self.process.terminate()
        await asyncio.wait_for(self.process.wait(), 5)
        await self.reader

    def events(self, session, start=0, method=None):
        return [f for f in self.frames[start:] if f.get('stream') == session and (method is None or f.get('method') == method)]

    async def approve(self, session, start):
        for f in self.events(session, start, 'approval.request'):
            p = f['params']
            ident = (session, p['approval_id'])
            if ident in self.approvals: continue
            # The test grants only workspace file operations and deterministic
            # Python/shell checks. Other tools stop the run for inspection.
            encoded = json.dumps(p.get('input', {})).lower()
            tool = p.get('tool', '').lower()
            forbidden = ('curl ', 'wget ', 'sudo ', 'ssh ', 'rm -', '.codex', '.ssh', 'auth.json', 'settings.json')
            if any(word in encoded for word in forbidden) or not any(word in tool for word in ('bash', 'shell', 'exec', 'write', 'edit', 'patch', 'read')):
                raise RuntimeError('Approval outside the deterministic test scope; inspect private event journal')
            self.approvals.add(ident)
            await self.rpc('approval.decide', session_id=session, approval_id=p['approval_id'], decision='allow', scope='once')

    async def turn(self, session, prompt, expected, reconnect=False, replay=False):
        start = len(self.frames)
        params = dict(session_id=session, message=prompt, client_msg_id=str(uuid.uuid4()))
        deadline = time.monotonic() + 60
        while True:
            try:
                result = await self.rpc('turn.start', **params)
                break
            except RuntimeError as e:
                if not any(message in str(e) for message in ('thread is still opening', 'interrupted Claude turn is still closing')) or time.monotonic() > deadline: raise
                await asyncio.sleep(.2)
        if replay:
            duplicate = await self.rpc('turn.start', **params)
            assert duplicate == result, 'Duplicate message changed acceptance receipt'
        if reconnect:
            before = await self.rpc('machine.describe')
            await self.disconnect()
            await self.connect()
            assert (await self.rpc('machine.describe'))['instance_id'] == before['instance_id']
            await self.rpc('session.subscribe', session_id=session, after_seq=0)
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            await self.approve(session, start)
            complete = self.events(session, start, 'turn.completed')
            if complete and complete[-1]['params'].get('outcome') != 'ok':
                raise RuntimeError('Turn failed: ' + json.dumps(complete[-1]['params']))
            settled = self.events(session, start, 'commit.settled')
            if complete and settled:
                text = '\n'.join(f['params'].get('event', {}).get('text') or '' for f in self.events(session, start, 'item.completed') if f['params'].get('event', {}).get('kind') == 'assistant_reply')
                assert text, 'No authoritative assistant response observed'
                if expected.startswith('MEMORY_'):
                    assert expected in text, f'Conversation context marker missing: {expected}'
                tools = [f for f in self.events(session, start, 'item.completed') if f['params'].get('event', {}).get('kind') in ('tool_call', 'tool_use', 'tool_result')]
                assert tools, 'No authoritative tool events observed'
                return dict(turn=result, settled=settled[-1]['params'], tool_events=len(tools), answer_marker_matched=expected in text)
            await asyncio.sleep(.15)
        raise TimeoutError('Turn did not complete and settle within the test deadline')


async def main(args):
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix='agd-matrix-', dir='/tmp')).resolve()
    binary = str(Path(args.binary).resolve())
    agit_home = Path(args.agit_home).expanduser().resolve() if args.agit_home else root / 'agit'
    endpoint = agit_home / 'desktop-rc/control.rpc'
    if endpoint.exists():
        try:
            _, probe = await asyncio.open_unix_connection(str(endpoint))
        except ConnectionRefusedError:
            endpoint.unlink()
        else:
            probe.close()
            await probe.wait_closed()
            raise RuntimeError('Refusing to replace an existing daemon; choose an isolated home or stop an idle test daemon first')
    env = dict(os.environ, AGIT_HOME=str(agit_home))
    if args.gateway:
        import sys
        key = sys.stdin.readline().strip()
        if not key: raise RuntimeError('Gateway credential required on stdin')
        env.update(ANTHROPIC_BASE_URL=args.gateway.removesuffix('/v1').rstrip('/'),
                   ANTHROPIC_AUTH_TOKEN=key, ANTHROPIC_MODEL=args.claude_model,
                   CLAUDE_CONFIG_DIR=str(root / 'claude'), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('ANTHROPIC_API_KEY', None)
        env.pop('CLAUDE_CODE_OAUTH_TOKEN', None)
        (root / 'claude').mkdir()
    log = (root / 'daemon.log').open('w')
    daemon = subprocess.Popen([binary, 'rc', 'local', 'start'], env=env, stdout=log, stderr=log, stdin=subprocess.DEVNULL, start_new_session=True)
    client = Client(binary, env, root)
    report = dict(root=str(root), agit_home=str(agit_home), daemon_pid=daemon.pid, started_at=time.strftime('%Y-%m-%dT%H:%M:%S%z'), sessions=[], checks=[], failures=[])
    print('Evidence:', root, flush=True)
    def save(): (root / 'report.json').write_text(json.dumps(report, indent=2))
    try:
        for _ in range(600):
            if (agit_home / 'desktop-rc/control.rpc').exists(): break
            if daemon.poll() is not None: raise RuntimeError('Daemon exited before readiness')
            await asyncio.sleep(.05)
        await client.connect()
        report['machine'] = await client.rpc('machine.describe')
        harnesses = [('codex', args.codex_model), ('claude-code', args.claude_model)]
        if args.runtime: harnesses = [h for h in harnesses if h[0] == args.runtime]
        for index in range(args.projects):
            project = f'matrix-{index + 1}'
            work = root / project
            work.mkdir()
            subprocess.run(['git', 'init', '-q', str(work)], check=True)
            (work / 'README.md').write_text('Isolated desktop integration test workspace.\n')
            await client.rpc('project.bind', workspace_id='local-owner', project_id=project, local_path=str(work))
            for runtime, model in harnesses:
                launch = dict(workspace_id='local-owner', project_id=project, runtime=runtime, model=model, start_id=str(uuid.uuid4()))
                response = await client.rpc('session.start', **launch)
                session = response['session']
                assert (await client.rpc('session.start', **launch))['session']['session_id'] == session['session_id']
                report['sessions'].append(dict(info=session, project=project, path=str(work), rounds=[]))
                print('Opened:', project, runtime, session['session_id'], flush=True)
        save()
        semaphore = asyncio.Semaphore(args.concurrency)
        async def exercise(item):
            async with semaphore:
                s = item['info']; session = s['session_id']; prefix = s['runtime'].replace('-', '_')
                nonce = 'MEMORY_' + uuid.uuid4().hex[:12]
                name = prefix + '_fixture.json'
                prompts = [
                    f'You are testing a desktop coding client in an isolated repository. Work only in this current directory. Do not use networking or touch credentials/configuration. Remember this conversation-only marker: {nonce}; do not save it to any file. Use tools to write {name} containing exactly {{"values":[2,3,5],"round":1}}. Then run a Python assertion checking that values sum to 10. Reply with ROUND_1_OK after the assertion passes.',
                    f'Continue the same task. Use tools to read {name}, append 7 to values, set round to 2, and save. Run Python assertions for the exact values [2,3,5,7], round == 2 and sum == 17. Work only in this directory. Reply with ROUND_2_OK.',
                    f'Use a tool to read and verify {name} is still round 2 with sum 17. Do not modify files. Recall the conversation-only marker from my first message and include it in your final answer followed by ROUND_3_OK.',
                ]
                try:
                    for index, prompt in enumerate(prompts):
                        result = await client.turn(session, prompt, nonce if index == 2 else f'ROUND_{index + 1}_OK', replay=index == 0)
                        value = json.loads((Path(item['path']) / name).read_text())
                        assert value == dict(values=[2, 3, 5] if index == 0 else [2, 3, 5, 7], round=1 if index == 0 else 2), 'Workspace artifact mismatch'
                        repository = agit_home / 'repos' / s['agent']
                        head = subprocess.check_output(['git', '-C', str(repository), 'rev-parse', 'refs/heads/' + s['branch']], text=True).strip()
                        assert head == result['settled']['commit_sha'], 'Settlement SHA does not match the actual branch'
                        assert not subprocess.check_output(['git', '-C', str(repository), 'remote'], text=True).strip()
                        item['rounds'].append(result)
                        print('PASS:', item['project'], s['runtime'], 'round', index + 1, 'tool events', result['tool_events'], flush=True)
                        save()
                except Exception as e:
                    report['failures'].append(dict(project=item['project'], runtime=s['runtime'], error=str(e)))
                    print('FAIL:', item['project'], s['runtime'], str(e), flush=True)
                    save()
        await asyncio.gather(*(exercise(s) for s in report['sessions']))
        before = await client.rpc('machine.describe')
        await client.disconnect(); await client.connect()
        assert (await client.rpc('machine.describe'))['instance_id'] == before['instance_id']
        for item in report['sessions']:
            session = item['info']['session_id']
            replay = await client.rpc('session.subscribe', session_id=session, after_seq=0)
            assert replay['session']['session_id'] == session
        report['checks'].append('All sessions survived a bridge reconnect and replayed on the original daemon')
        print('PASS: reconnect and all-session replay', flush=True)
    except Exception as e:
        report['failures'].append(dict(error=str(e)))
        print('FAIL:', str(e), flush=True)
    finally:
        save()
        if hasattr(client, 'process'): await client.disconnect()
        client.journal.close()
        if not args.keep:
            subprocess.run([binary, 'rc', 'local', 'stop'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            try: daemon.wait(timeout=30)
            except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        else:
            print('Kept test daemon and sessions for desktop verification:', daemon.pid, flush=True)
        log.close()
    return 1 if report['failures'] else 0


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--gateway')
    parser.add_argument('--agit-home', help='Explicit owner state directory for desktop-visible tests')
    parser.add_argument('--keep', action='store_true', help='Leave this test daemon running for desktop verification')
    parser.add_argument('--runtime', choices=['codex', 'claude-code'])
    parser.add_argument('--codex-model', default='gpt-5.3-codex-spark')
    parser.add_argument('--claude-model', default='glm-5.3-nvfp4')
    parser.add_argument('--projects', type=int, default=3)
    parser.add_argument('--concurrency', type=int, default=2)
    raise SystemExit(asyncio.run(main(parser.parse_args())))
