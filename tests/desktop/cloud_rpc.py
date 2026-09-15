"""Exercise the backend relay with real daemons, OS workers, and a fixture harness.

The backend's opt-in cloud_peer_process_chain test supplies a disposable account
token through stdin. Normal login and enrollment commands own credential files.
"""
import asyncio
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import time
import uuid


HARNESS = r'''#!/usr/bin/env python3
import json, os, sys, uuid
from pathlib import Path
if "app-server" not in sys.argv:
    print("codex-cli 0.0.0-test")
    sys.exit(0)
def emit(value):
    print(json.dumps(value), flush=True)
def record(kind, payload):
    with transcript.open("a") as file:
        file.write(json.dumps({"type":kind,"timestamp":"2026-09-15T00:00:00Z","payload":payload}) + "\n")
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    params = request.get("params", {})
    result = {}
    if method in ("thread/start", "thread/resume"):
        native = params.get("threadId") or str(uuid.uuid4())
        directory = Path(os.environ["CODEX_HOME"]) / "sessions" / "2026" / "09" / "15"
        directory.mkdir(parents=True, exist_ok=True)
        transcript = directory / ("rollout-2026-09-15T00-00-00-" + native + ".jsonl")
        record("session_meta", {"id":native,"cwd":os.getcwd(),"originator":"codex_cli_rs","cli_version":"0.0.0-test"})
        result = {"thread":{"id":native}}
    elif method == "turn/start":
        turn = str(uuid.uuid4())
        emit({"id":request["id"],"result":{"turn":{"id":turn}}})
        emit({"method":"turn/started","params":{"threadId":native,"turn":{"id":turn}}})
        record("event_msg", {"type":"user_message","message":"relay fixture request"})
        record("response_item", {"type":"message","role":"assistant","content":[{"type":"output_text","text":"CLOUD_PROCESS_REPLY"}]})
        emit({"method":"item/completed","params":{"threadId":native,"turnId":turn,"item":{"id":str(uuid.uuid4()),"type":"agentMessage","text":"CLOUD_PROCESS_REPLY"}}})
        emit({"method":"turn/completed","params":{"threadId":native,"turn":{"id":turn,"status":"completed"}}})
        continue
    emit({"id":request["id"],"result":result})
'''


class Client:
    async def connect(self, binary, env, journal):
        self.process = await asyncio.create_subprocess_exec(
            binary, "rc", "local", "bridge", env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL, limit=8 * 1024 * 1024)
        self.pending, self.frames, self.route = {}, [], {}
        self.journal = journal
        self.pump = asyncio.create_task(self.read())
        return self

    async def read(self):
        try:
            while line := await self.process.stdout.readline():
                frame = json.loads(line)
                future = self.pending.pop(frame.get("id"), None)
                if future is not None and not future.done():
                    future.set_result(frame)
                elif "method" in frame:
                    self.frames.append(frame)
                    self.journal.write(json.dumps(frame) + "\n")
                    self.journal.flush()
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError("owner bridge closed"))

    async def raw(self, rpc_method, **params):
        identity = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[identity] = future
        self.process.stdin.write((json.dumps(dict(jsonrpc="2.0", id=identity, method=rpc_method, params=params)) + "\n").encode())
        await self.process.stdin.drain()
        return await asyncio.wait_for(future, 75)

    async def rpc(self, rpc_method, **params):
        frame = await self.raw(rpc_method, **params)
        assert "error" not in frame, frame
        if rpc_method == "peer.connect_cloud":
            self.route = {key: frame["result"][key] for key in ("route_id", "generation")}
        return frame["result"]

    async def peer(self, method, **params):
        params.setdefault("workspace_id", "local-owner")
        return await self.rpc("peer.request", peer_id="target", **self.route, method=method, params=params)

    def events(self, session, method):
        return [event for frame in self.frames
                if frame.get("method") == "peer.frame"
                and (event := frame["params"]["frame"]).get("stream") == session
                and event.get("method") == method]

    async def close(self):
        self.process.stdin.close()
        try:
            await asyncio.wait_for(self.process.wait(), 5)
        except asyncio.TimeoutError:
            self.process.kill()
            await self.process.wait()
        await self.pump


async def eventually(check, description, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = await check()
        if result:
            return result
        await asyncio.sleep(0.1)
    raise AssertionError(description)


async def command(binary, env, *args, input=None):
    process = await asyncio.create_subprocess_exec(binary, *args, env=env,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    stdout, stderr = await asyncio.wait_for(process.communicate(input), 30)
    assert process.returncode == 0, stderr.decode()
    return stdout.decode()


async def run(binary, hub, account, token):
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix="agd-cloud-", dir="/tmp")).resolve()
    print("Cloud process evidence:", root, flush=True)
    (root / "bin").mkdir()
    harness = root / "bin" / "codex"
    harness.write_text(HARNESS)
    harness.chmod(0o700)
    project = root / "project"
    project.mkdir()
    base = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"], SHELL="/bin/bash")
    for key in ("AGIT_SESSION", "AGIT_MERGE_TX", "AGIT_RC"):
        base.pop(key, None)
    environments = [dict(base, AGIT_HOME=str(root / name), CODEX_HOME=str(root / (name + "-codex")))
                    for name in ("controller", "executor")]
    daemons, clients, logs = [], [], []
    try:
        for name, env in zip(("controller", "executor"), environments):
            await command(binary, env, "login", "--hub", hub, "--with-token", input=token.encode())
            log = (root / (name + ".log")).open("w")
            logs.append(log)
            daemon = await asyncio.create_subprocess_exec(binary, "rc", "local", "start", env=env, stdout=log, stderr=log)
            daemons.append(daemon)
            async def ready():
                assert daemon.returncode is None, "isolated daemon exited"
                return (Path(env["AGIT_HOME"]) / "desktop-rc" / "control.rpc").exists()
            await eventually(ready, "owner endpoint did not open")
            journal = (root / (name + "-events.jsonl")).open("w")
            logs.append(journal)
            client = await Client().connect(binary, env, journal)
            clients.append(client)
            if name == "executor":
                await client.rpc("peer.cloud", operation="enroll", hub=hub, name=name)
        controller, executor = clients
        local = await controller.rpc("machine.describe")
        target = await executor.rpc("machine.describe")
        source_status = await controller.rpc("peer.cloud", operation="status", hub=hub)
        assert source_status["device"] is None
        device = (await executor.rpc("peer.cloud", operation="status", hub=hub))["device"]
        async def online():
            page = await controller.rpc("peer.cloud", operation="devices", hub=hub)
            return any(row["device"]["id"] == device["id"] and row["online"] for row in page["devices"])
        await eventually(online, "outbound executor presence did not register")
        config = dict(peer_id="target", hub=hub, target=device)
        connected = await controller.rpc("peer.connect_cloud", **config)
        assert connected["description"]["instance_id"] == target["instance_id"]
        assert connected["description"]["authority"] == "cloud-principal"
        assert "diagnostic_log" not in connected["description"]
        assert local["instance_id"] != target["instance_id"]
        await controller.peer("project.bind", project_id="fixture", local_path=str(project))
        assert "fixture" in json.dumps(await controller.peer("workspace.list"))
        start = dict(project_id="fixture", runtime="codex", start_id=str(uuid.uuid4()))
        opened = await controller.peer("session.start", **start)
        session = opened["session"]["session_id"]
        assert (await controller.peer("session.start", **start)) == opened
        await controller.peer("session.subscribe", session_id=session, after_seq=0)
        message = dict(session_id=session, message="relay fixture request", client_msg_id=str(uuid.uuid4()))
        receipt = await controller.peer("turn.start", **message)
        assert await controller.peer("turn.start", **message) == receipt
        async def completed():
            return controller.events(session, "turn.completed")
        await eventually(completed, "harness completion did not cross relay")
        async def history_ready():
            history = await controller.peer("session.history", session_id=session)
            return history if "CLOUD_PROCESS_REPLY" in json.dumps(history) else None
        await eventually(history_ready, "harness transcript did not reach cloud history")
        first = (await controller.rpc("peer.list"))["peers"][0]
        assert first["worker_pid"] not in [daemon.pid for daemon in daemons]
        os.kill(first["worker_pid"], signal.SIGKILL)
        assert (await controller.rpc("machine.describe"))["instance_id"] == local["instance_id"]
        async def recovered():
            status = (await controller.rpc("peer.list"))["peers"][0]
            return status if status["state"] == "online" and status["generation"] > first["generation"] else None
        restored = await eventually(recovered, "cloud worker did not reconnect")
        assert restored["worker_pid"] != first["worker_pid"]
        assert restored["description"]["instance_id"] == target["instance_id"]
        stale = await controller.raw("peer.request", peer_id="target", **controller.route, method="turn.start", params=message)
        assert stale["error"]["data"]["outcome"] == "not_sent", stale
        await controller.rpc("peer.connect_cloud", **config)
        await controller.peer("session.list", include_local=False)
        before = len(controller.events(session, "turn.completed"))
        await controller.peer("session.subscribe", session_id=session, after_seq=0)
        async def replayed():
            return len(controller.events(session, "turn.completed")) > before
        await eventually(replayed, "session replay did not survive worker replacement")
        if desktop_test := os.environ.get("AGIT_PEER_DESKTOP_TEST"):
            desktop_env = dict(environments[0],
                AGD_TEST_CLOUD_MACHINE=json.dumps(dict(id="desktop-cloud-test", name="Cloud fixture", kind="cloud",
                    binary="@bundled", cloud=dict(hub=hub, target=device))),
                AGD_TEST_CLOUD_SESSION=session)
            desktop = await asyncio.create_subprocess_exec(desktop_test, "cloud_bridge_ipc", "--ignored", "--nocapture", env=desktop_env)
            assert await asyncio.wait_for(desktop.wait(), 60) == 0, "Desktop cloud IPC failed"
        await command(binary, environments[1], "rc", "cloud", "grant", "--hub", hub,
                      "--account", account, "--resource", "session:" + session, "--access", "deny")
        async def denied():
            reply = await controller.raw("peer.request", peer_id="target", **controller.route,
                                         method="session.history", params=dict(session_id=session))
            return reply if "error" in reply else None
        await eventually(denied, "executor session denial did not take effect")
        catalog = await controller.peer("session.list", include_local=False)
        assert not any(row["session_id"] == session for row in catalog["sessions"])
        assert (await executor.rpc("machine.describe"))["instance_id"] == target["instance_id"]
        print("PASS: enrollment, outbound presence, encrypted relay, distinct daemon and worker processes", flush=True)
        print("PASS: native launch, turn receipt, events, history, worker recovery, session replay and executor denial", flush=True)
    finally:
        for client in clients:
            await client.close()
        for env in environments:
            try:
                await command(binary, env, "rc", "local", "stop")
            except (AssertionError, asyncio.TimeoutError):
                pass
        for daemon in daemons:
            try:
                await asyncio.wait_for(daemon.wait(), 15)
            except asyncio.TimeoutError:
                daemon.kill()
                await daemon.wait()
        for env in environments:
            home = Path(env["AGIT_HOME"])
            for diagnostic in home.glob("desktop-rc/*.jsonl"):
                shutil.copy2(diagnostic, root / (home.name + "-" + diagnostic.name))
            shutil.rmtree(home, ignore_errors=True)
        for log in logs:
            log.close()


if __name__ == "__main__":
    asyncio.run(run(str(Path(sys.argv[1]).resolve()), sys.argv[2], sys.argv[3], sys.stdin.read().strip()))
