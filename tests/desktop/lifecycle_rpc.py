"""Exercise interruption, reconnect, and continuation of matrix test sessions."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import sys
import time
import uuid
from matrix_rpc import Client


async def run(binary, report_path, runtime_filter=None):
    path = Path(report_path)
    matrix = json.loads(path.read_text())
    root = path.parent
    env = dict(os.environ, AGIT_HOME=matrix.get('agit_home', str(root / 'agit')))
    client = Client(str(Path(binary).resolve()), env, root)
    report = {'checks': [], 'failures': []}
    evidence = root / f'lifecycle-{runtime_filter or "all"}-{time.time_ns()}.json'
    await client.connect()
    try:
        for item in matrix['sessions']:
            if item['project'] != 'matrix-1': continue
            session = item['info']['session_id']
            runtime = item['info']['runtime']
            if runtime_filter and runtime != runtime_filter: continue
            try:
                resumed = await client.rpc('session.resume', workspace_id='local-owner', session_id=session)
                assert resumed['session']['session_id'] == session
                start = len(client.frames)
                marker = runtime.replace('-', '_') + '_after_interrupt_' + uuid.uuid4().hex[:12] + '.txt'
                launched_at = time.monotonic()
                prompt = f'Run a shell tool now to execute exactly: python3 -c "import time; from pathlib import Path; print(\'WAITING_FOR_INTERRUPT\', flush=True); time.sleep(40); Path(\'{marker}\').write_text(\'too late\')". This is an interruption test in the current isolated repository. Do not replace the sleep with a shorter one or perform any other work.'
                intent = dict(session_id=session, message=prompt, client_msg_id=str(uuid.uuid4()))
                ready_deadline = time.monotonic() + 60
                while True:
                    try:
                        await client.rpc('turn.start', **intent)
                        break
                    except RuntimeError as error:
                        if 'thread is still opening' not in str(error) or time.monotonic() > ready_deadline: raise
                        await asyncio.sleep(.2)
                deadline = time.monotonic() + 90
                while time.monotonic() < deadline:
                    await client.approve(session, start)
                    tools = [f for f in client.events(session, start, 'item.completed') if f['params'].get('event', {}).get('kind') == 'tool_use']
                    live_tools = [f for f in client.events(session, start, 'item.started') if f['params'].get('kind') == 'tool_call']
                    if tools or live_tools: break
                    if client.events(session, start, 'turn.completed'): raise RuntimeError('Turn completed before a tool could be interrupted')
                    await asyncio.sleep(.1)
                else: raise TimeoutError('No tool started before the deadline')
                for _ in range(20):
                    await client.approve(session, start)
                    await asyncio.sleep(.1)
                before = await client.rpc('machine.describe')
                await client.disconnect(); await client.connect()
                assert (await client.rpc('machine.describe'))['instance_id'] == before['instance_id']
                await client.rpc('session.subscribe', session_id=session, after_seq=max((f.get('seq',0) for f in client.events(session)),default=0))
                duplicate = await client.rpc('turn.start', **intent)
                assert duplicate.get('turn_id'), 'Reconnect lost the accepted message receipt'
                await client.rpc('turn.interrupt', session_id=session)
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    complete = client.events(session, start, 'turn.completed')
                    if complete: break
                    await asyncio.sleep(.1)
                assert complete and complete[-1]['params']['outcome'] == 'interrupted', 'Interrupt did not produce an interrupted turn'
                assert not (Path(item['path']) / marker).exists(), 'The interrupted command reached its write'
                fixture = runtime.replace('-', '_') + '_fixture.json'
                result = await client.turn(session, f'Continue after the interrupted command. Do not rerun it. Use a tool to read {fixture} and verify round == 2 and values sum to 17. Reply RECOVERY_OK. Work only in this repository.', 'RECOVERY_OK')
                assert result['answer_marker_matched'], 'Recovery response did not acknowledge completion'
                await asyncio.sleep(max(0, 45 - (time.monotonic() - launched_at)))
                background = 'completed after turn interruption' if (Path(item['path']) / marker).exists() else 'stopped'
                if runtime != 'codex': assert background == 'stopped', 'A detached tool wrote after interruption'
                report['checks'].append({'runtime':runtime,'result':'passed','background_process':background,'cases':['resume existing identity','tool start','disconnect while running','same-instance reconnect','message receipt replay','interrupt','background process observed separately from turn cancellation','next turn and settlement']})
                print('PASS lifecycle:', runtime, flush=True)
            except Exception as error:
                report['failures'].append({'runtime':runtime,'error':str(error)})
                print('FAIL lifecycle:', runtime, str(error), flush=True)
            content = json.dumps(report, indent=2)
            evidence.write_text(content)
            (root / 'lifecycle-report.json').write_text(content)
    finally:
        await client.disconnect()
        client.journal.close()
    return bool(report['failures'])


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary'); parser.add_argument('report')
    parser.add_argument('--runtime', choices=['codex', 'claude-code'])
    args = parser.parse_args()
    raise SystemExit(asyncio.run(run(args.binary, args.report, args.runtime)))
