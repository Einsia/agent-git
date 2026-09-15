"""Rerun matrix turns on existing desktop sessions, optionally after restart."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid
from matrix_rpc import Client


async def run(args):
    path = Path(args.report)
    matrix = json.loads(path.read_text())
    root = path.parent
    binary = str(Path(args.binary).resolve())
    env = dict(os.environ, AGIT_HOME=matrix.get('agit_home', str(root / 'agit')))
    if args.gateway:
        key = sys.stdin.readline().strip()
        if not key: raise RuntimeError('Gateway credential required on stdin')
        env.update(ANTHROPIC_AUTH_TOKEN=key, ANTHROPIC_BASE_URL=args.gateway.removesuffix('/v1').rstrip('/'),
                   ANTHROPIC_MODEL='glm-5.3-nvfp4', CLAUDE_CONFIG_DIR=str(root / 'claude'), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('ANTHROPIC_API_KEY', None); env.pop('CLAUDE_CODE_OAUTH_TOKEN', None)
    client = Client(binary, env, root)
    await client.connect()
    if args.restart:
        live = await client.rpc('session.list', workspace_id='local-owner', include_local=False)
        expected = {s['info']['session_id'] for s in matrix['sessions']}
        assert all(s['session_id'] in expected for s in live['sessions']), 'A non-test session exists; refusing daemon restart'
        old = (await client.rpc('machine.describe'))['instance_id']
        old_pid = int((Path(env['AGIT_HOME']) / 'desktop-rc/agitd.pid').read_text().strip())
        await client.disconnect()
        subprocess.run([binary,'rc','local','stop'],env=env,check=True,capture_output=True,timeout=45)
        endpoint = Path(env['AGIT_HOME']) / 'desktop-rc/control.rpc'
        for _ in range(300):
            try: _, writer = await asyncio.open_unix_connection(str(endpoint))
            except (ConnectionRefusedError,FileNotFoundError): break
            writer.close(); await writer.wait_closed(); await asyncio.sleep(.1)
        else: raise TimeoutError('Old daemon RPC did not stop')
        for _ in range(600):
            try: os.kill(old_pid, 0)
            except ProcessLookupError: break
            await asyncio.sleep(.1)
        else: raise TimeoutError('Old daemon process did not finish session shutdown')
        with (root / 'restart-daemon.log').open('w') as log:
            daemon = subprocess.Popen([binary,'rc','local','start'],env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
        for _ in range(300):
            if daemon.poll() is not None: raise RuntimeError('Test daemon exited during startup; another client may have started a daemon with different environment')
            try: _, writer = await asyncio.open_unix_connection(str(endpoint))
            except (ConnectionRefusedError,FileNotFoundError): await asyncio.sleep(.1); continue
            writer.close(); await writer.wait_closed(); break
        await client.connect()
        assert (await client.rpc('machine.describe'))['instance_id'] != old
        print('PASS: daemon restarted with a new instance identity',flush=True)
    report = {'sessions':[], 'failures':[], 'restarted':args.restart}
    prefix = 'restart' if args.restart else 'continuation'
    target = root / f'{prefix}-{args.runtime or "all"}-{time.time_ns()}.json'
    def save():
        content = json.dumps(report,indent=2)
        target.write_text(content)
        (root / f'{prefix}-report.json').write_text(content)
    semaphore = asyncio.Semaphore(2)
    async def exercise(item):
        async with semaphore:
            s = item['info']; sid=s['session_id']; runtime=s['runtime']; rounds=[]
            try:
                deadline=time.monotonic()+150
                while True:
                    try:
                        result=await client.rpc('session.resume',workspace_id='local-owner',session_id=sid)
                        assert result['session']['session_id']==sid
                        break
                    except RuntimeError as error:
                        if ('busy' not in str(error).lower() and 'recently' not in str(error).lower() and 'active' not in str(error).lower() and 'still open in a terminal' not in str(error).lower()) or time.monotonic()>deadline: raise
                        await asyncio.sleep(1)
                print('Resumed:',item['project'],runtime,flush=True)
                for attempt in range(150):
                    try:
                        await client.rpc('session.setPermissionMode',session_id=sid,mode='default')
                        break
                    except RuntimeError as error:
                        if 'still proving its fail-closed Plan restart' not in str(error) or attempt == 149: raise
                        await asyncio.sleep(1)
                name=runtime.replace('-','_')+'_fixture.json';nonce='MEMORY_'+uuid.uuid4().hex[:12]
                prompts=[
                    f'This is a fresh verification sequence in the same isolated test repository. Work only here. Remember the new conversation-only marker {nonce}; do not write the marker to any file. Use tools to overwrite {name} with exactly {{"values":[2,3,5],"round":1}} and run Python assertions that values sum to 10. Reply ROUND_1_OK.',
                    f'Use tools to read {name}, append 7 to values, change round to 2, save, and run Python assertions for values [2,3,5,7], round 2, and sum 17. Reply ROUND_2_OK.',
                    f'Use a tool to verify {name} still has round 2 and sum 17. Recall the new conversation-only MEMORY marker from this verification sequence and include it in your final answer. Do not modify any files.',
                ]
                for index,prompt in enumerate(prompts):
                    result=await client.turn(sid,prompt,nonce if index==2 else f'ROUND_{index+1}_OK',replay=index==0)
                    value=json.loads((Path(item['path'])/name).read_text())
                    assert value==dict(values=[2,3,5] if index==0 else [2,3,5,7],round=1 if index==0 else 2)
                    repo=Path(env['AGIT_HOME'])/'repos'/s['agent']
                    head=subprocess.check_output(['git','-C',str(repo),'rev-parse','refs/heads/'+s['branch']],text=True).strip()
                    assert head==result['settled']['commit_sha']
                    rounds.append(result);print('PASS:',item['project'],runtime,'round',index+1,flush=True)
            except Exception as error:
                report['failures'].append(dict(session_id=sid,runtime=runtime,error=str(error)))
                print('FAIL:',item['project'],runtime,str(error),flush=True)
            report['sessions'].append(dict(session_id=sid,runtime=runtime,project=item['project'],rounds=rounds));save()
    try:
        selected = [item for item in matrix['sessions'] if args.runtime is None or item['info']['runtime'] == args.runtime]
        assert selected, 'No sessions match the requested runtime'
        await asyncio.gather(*(exercise(item) for item in selected))
        before=(await client.rpc('machine.describe'))['instance_id']
        await client.disconnect();await client.connect()
        assert (await client.rpc('machine.describe'))['instance_id']==before
        for item in selected:
            await client.rpc('session.subscribe',session_id=item['info']['session_id'],after_seq=0)
        report['replay']='passed';save();print('PASS: all-session replay',flush=True)
    finally:
        await client.disconnect();client.journal.close();save()
    return bool(report['failures'])


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--runtime', choices=['codex', 'claude-code']);p.add_argument('binary');p.add_argument('report');p.add_argument('--restart',action='store_true');p.add_argument('--gateway')
    raise SystemExit(asyncio.run(run(p.parse_args())))
