"""Real model test through the owner daemon, with process-only gateway credentials.

Usage: model_rpc.py AGIT_BINARY RUNTIME MODEL [GATEWAY_BASE]
A gateway credential, when needed, is read from stdin and never printed.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import uuid

binary, runtime, model = sys.argv[1:4]
binary = str(Path(binary).resolve())
base = sys.argv[4] if len(sys.argv) > 4 else None
key = sys.stdin.readline().strip() if base else None
if base and not key: raise SystemExit('Gateway credential is required on stdin')
root = Path(tempfile.mkdtemp(prefix="agd-model-", dir="/tmp"))
work = root / "project"
work.mkdir()
env = dict(os.environ, AGIT_HOME=str(root / "agit"))
if base:
    env.update(ANTHROPIC_BASE_URL=base.removesuffix('/v1').rstrip('/'), ANTHROPIC_AUTH_TOKEN=key,
               CLAUDE_CONFIG_DIR=str(root/'claude'), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
    env.pop('ANTHROPIC_API_KEY', None)
    env.pop('CLAUDE_CODE_OAUTH_TOKEN', None)
    (root/'claude').mkdir()
log = (root / 'daemon.log').open('w')
daemon = subprocess.Popen([binary, 'rc', 'local', 'start'], env=env, stdout=log, stderr=log)
endpoint = root/'agit/desktop-rc/control.rpc'
client = None
reader = None
frames = []
try:
    for _ in range(600):
        if endpoint.exists(): break
        if daemon.poll() is not None: raise RuntimeError('daemon exited before readiness')
        time.sleep(.05)
    def attach():
        sock = socket.socket(socket.AF_UNIX); sock.settimeout(180); sock.connect(str(endpoint))
        return sock, sock.makefile('rb')
    client, reader = attach()
    def next_frame():
        line = reader.readline()
        if not line: raise RuntimeError('RPC connection closed')
        frame = json.loads(line)
        if frame.get('method'): frames.append(frame)
        return frame
    def rpc(method, params):
        request_id = str(uuid.uuid4())
        client.sendall((json.dumps(dict(jsonrpc='2.0',id=request_id,method=method,params=params))+'\n').encode())
        while True:
            response = next_frame()
            if response.get('id') == request_id:
                if 'error' in response: raise RuntimeError(json.dumps(response['error']))
                return response['result']
    description = rpc('machine.describe', {})
    rpc('project.bind', dict(workspace_id='local-owner',project_id='model-test',local_path=str(work)))
    start = dict(workspace_id='local-owner',project_id='model-test',runtime=runtime,model=model,start_id=str(uuid.uuid4()))
    result = rpc('session.start', start)
    session = result['session']
    duplicate = rpc('session.start', start)
    assert duplicate['session']['session_id'] == session['session_id']
    print('PASS: keyed start replayed the same session', flush=True)
    prompt = 'Reply with exactly AGENTGIT_DESKTOP_OK. Do not use any tools.'
    message = dict(session_id=session['session_id'],message=prompt,client_msg_id=str(uuid.uuid4()))
    ready_deadline = time.monotonic() + 30
    while True:
        try:
            turn = rpc('turn.start', message)
            break
        except RuntimeError as error:
            if 'thread is still opening' not in str(error) or time.monotonic() > ready_deadline: raise
            time.sleep(.2)
    print('Turn accepted:', json.dumps(turn), flush=True)
    old_instance = description['instance_id']
    reader.close(); client.close()
    time.sleep(1)
    client, reader = attach()
    assert rpc('machine.describe', {})['instance_id'] == old_instance
    rpc('session.subscribe',dict(session_id=session['session_id'],after_seq=0))
    print('PASS: reconnected to the original daemon', flush=True)
    deadline = time.monotonic()+240
    while time.monotonic()<deadline:
        if any(f.get('method')=='commit.settled' for f in frames): break
        frame = next_frame()
        if frame.get('method')=='turn.completed':
            print('Turn completed:',json.dumps(frame.get('params')),flush=True)
            if frame.get('params',{}).get('outcome')=='error': raise RuntimeError('model turn failed')
    assert any('AGENTGIT_DESKTOP_OK' in json.dumps(f.get('params',{})) for f in frames), 'model response not observed'
    settled = [f for f in frames if f.get('method')=='commit.settled']
    assert settled, 'completed turn did not settle locally'
    repository = root/'agit/repos'/session['agent']
    branch = session['branch']
    head = subprocess.check_output(['git','-C',str(repository),'rev-parse',f'refs/heads/{branch}'],text=True).strip()
    assert settled[-1]['params']['commit_sha'] == head
    remotes = subprocess.check_output(['git','-C',str(repository),'remote'],text=True).strip()
    assert not remotes, 'local settlement configured a remote'
    print('PASS: real model output, detached reconnect, local Git settlement without publication',flush=True)
    print('Evidence directory:',root,flush=True)
    (root/'events.json').write_text(json.dumps(frames,ensure_ascii=False,indent=2))
finally:
    if reader: reader.close()
    if client: client.close()
    subprocess.run([binary,'rc','local','stop'],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=5)
    try: daemon.wait(timeout=30)
    except subprocess.TimeoutExpired: daemon.kill();daemon.wait()
    log.close()
    print('Test diagnostics:',root/'daemon.log',flush=True)
