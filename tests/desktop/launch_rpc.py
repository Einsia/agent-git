"""Verify start/resume arbitration through real owner RPC with a synthetic harness."""
import asyncio
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid


HARNESS = r'''#!/usr/bin/env python3
import json, os, sys, uuid
if "app-server" not in sys.argv:
    print("codex-cli 0.0.0-test")
    sys.exit(0)
with open(os.environ["AGIT_TEST_LAUNCH_LOG"], "a") as log:
    log.write(json.dumps({"event": "spawn", "pid": os.getpid()}) + "\n")
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    result = {}
    if method in ("thread/start", "thread/resume"):
        native = request.get("params", {}).get("threadId") or str(uuid.uuid4())
        result = {"thread": {"id": native}}
        with open(os.environ["AGIT_TEST_LAUNCH_LOG"], "a") as log:
            log.write(json.dumps({"event": "thread", "pid": os.getpid(), "id": native, "method": method}) + "\n")
    print(json.dumps({"id": request["id"], "result": result}), flush=True)
'''


class Client:
    async def connect(self, endpoint):
        self.reader, self.writer = await asyncio.open_unix_connection(endpoint)
        self.pending = {}
        self.pump = asyncio.create_task(self.read())
        return self

    async def read(self):
        try:
            while line := await self.reader.readline():
                frame = json.loads(line)
                future = self.pending.pop(frame.get("id"), None)
                if future is not None and not future.done():
                    future.set_result(frame)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError("owner RPC closed"))

    async def rpc(self, rpc_method, **params):
        if not rpc_method.startswith("peer."):
            params.setdefault("workspace_id", "local-owner")
        ident = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        self.writer.write((json.dumps(dict(jsonrpc="2.0", id=ident, method=rpc_method, params=params)) + "\n").encode())
        await self.writer.drain()
        return await asyncio.wait_for(future, 15)

    async def close(self):
        self.writer.close()
        await self.writer.wait_closed()
        await self.pump


async def eventually(check, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        result = await check()
        if result:
            return result
        await asyncio.sleep(0.05)
    raise AssertionError(description)


async def run(binary):
    with tempfile.TemporaryDirectory(prefix="agit-launch-", dir="/tmp") as directory:
        root = Path(directory)
        (root / "bin").mkdir()
        project = root / "project"
        project.mkdir()
        harness = root / "bin" / "codex"
        harness.write_text(HARNESS)
        harness.chmod(0o700)
        trace = root / "launches.jsonl"
        env = dict(os.environ, AGIT_HOME=str(root / "agit"), CODEX_HOME=str(root / "codex"),
                   AGIT_TEST_LAUNCH_LOG=str(trace), PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
        endpoint = root / "agit" / "desktop-rc" / "control.rpc"
        clients = []
        with (root / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
            try:
                async def ready():
                    assert daemon.poll() is None, (root / "daemon.log").read_text()
                    return endpoint.exists()
                await eventually(ready, "owner endpoint did not open")
                clients = [await Client().connect(str(endpoint)) for _ in range(2)]
                first, second = clients
                identity = (await first.rpc("machine.describe"))["result"]["instance_id"]
                bound = await first.rpc("project.bind", project_id="test", local_path=str(project))
                assert "result" in bound, bound
                params = dict(project_id="test", runtime="codex", start_id=str(uuid.uuid4()))
                replies = await asyncio.gather(first.rpc("session.start", **params), second.rpc("session.start", **params))
                assert any("result" in reply for reply in replies), replies
                completed = (await first.rpc("session.start", **params))["result"]
                session_id = completed["session"]["session_id"]
                assert all("error" in reply or reply["result"] == completed for reply in replies), replies

                def records(event):
                    return [item for line in trace.read_text().splitlines() if (item := json.loads(line))["event"] == event] if trace.exists() else []

                async def native_bound():
                    return records("thread")
                await eventually(native_bound, "native thread did not open")
                assert len(records("spawn")) == 1, "same start key launched duplicate harnesses"
                healthy = (await second.rpc("session.start", project_id="test", runtime="codex", start_id=str(uuid.uuid4())))["result"]["session"]["session_id"]
                async def both_bound():
                    return len(records("thread")) == 2
                await eventually(both_bound, "independent session did not open")
                os.kill(records("thread")[0]["pid"], signal.SIGKILL)
                async def only_healthy():
                    sessions = (await first.rpc("session.list", include_local=False))["result"]["sessions"]
                    return [item["session_id"] for item in sessions] == [healthy]
                await eventually(only_healthy, "dead harness remained live or removed an unrelated session")
                replies = await asyncio.gather(first.rpc("session.resume", session_id=session_id), second.rpc("session.resume", session_id=session_id))
                assert any("result" in reply for reply in replies), replies
                async def resumed():
                    return len(records("thread")) == 3
                await eventually(resumed, "resume did not reach the native harness")
                assert len(records("spawn")) == 3, "competing resumes launched duplicate harnesses"
                assert records("thread")[2]["id"] == records("thread")[0]["id"], "resume changed native identity"
                assert (await second.rpc("session.resume", session_id=session_id))["result"]["session"]["session_id"] == session_id
                assert len(records("spawn")) == 3
                assert (await first.rpc("machine.describe"))["result"]["instance_id"] == identity
                print("PASS: keyed start arbitration, concurrent resume, native identity, harness exit isolation, daemon continuity")
            except Exception:
                print((root / "daemon.log").read_text(), file=sys.stderr)
                raise
            finally:
                for client in clients:
                    await client.close()
                try:
                    subprocess.run([binary, "rc", "local", "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                try:
                    daemon.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    asyncio.run(run(str(Path(sys.argv[1]).resolve())))
