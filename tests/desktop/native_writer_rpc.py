"""Check real Codex writer refusal through direct and daemon-peer owner RPC."""

import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid

from launch_rpc import Client, eventually


class Native:
    async def start(self, env, log):
        self.process = await asyncio.create_subprocess_exec(
            "codex", "app-server", env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=log,
        )
        self.serial = 0
        response = await self.rpc("initialize", dict(
            clientInfo=dict(name="agit-writer-contract", version="0.0.0"),
            capabilities=dict(experimentalApi=True),
        ))
        assert "result" in response, response
        self.version = response["result"]["userAgent"]
        self.process.stdin.write(b'{"method":"initialized"}\n')
        await self.process.stdin.drain()
        return self

    async def rpc(self, method, params):
        self.serial += 1
        self.process.stdin.write((json.dumps(dict(id=self.serial, method=method, params=params)) + "\n").encode())
        await self.process.stdin.drain()
        while True:
            line = await asyncio.wait_for(self.process.stdout.readline(), 15)
            assert line, "native app-server exited before its reply"
            response = json.loads(line)
            if response.get("id") == self.serial:
                return response

    async def close(self):
        if self.process.returncode is not None:
            return
        self.process.stdin.close()
        try:
            await asyncio.wait_for(self.process.wait(), 10)
        except asyncio.TimeoutError:
            self.process.kill()
            await self.process.wait()


def fixture(root, native):
    home = root / "codex"
    home.mkdir()
    (home / "config.toml").write_text('''model = "fixture"
model_provider = "fixture"
[model_providers.fixture]
name = "fixture"
base_url = "http://127.0.0.1:1/v1"
wire_api = "responses"
''')
    stamp = "2026-09-15T00:00:00Z"
    transcript = home / "sessions" / "2026" / "09" / "15" / f"rollout-2026-09-15T00-00-00-{native}.jsonl"
    transcript.parent.mkdir(parents=True)
    records = [
        dict(type="session_meta", payload=dict(id=native, session_id=native, timestamp=stamp,
            cwd=str(root / "project"), originator="agit-native-contract-test", cli_version="0.153.2",
            source="cli", model_provider="fixture", history_mode="legacy")),
        dict(type="response_item", payload=dict(type="message", role="user",
            content=[dict(type="input_text", text="Synthetic history fixture.")])),
        dict(type="response_item", payload=dict(type="message", role="assistant",
            content=[dict(type="output_text", text="Synthetic fixture complete.")])),
    ]
    transcript.write_text("".join(json.dumps(dict(timestamp=stamp, **record)) + "\n" for record in records))
    return home, transcript


async def run(binary):
    with tempfile.TemporaryDirectory(prefix="ag-writer-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        (root / "project").mkdir()
        (root / "bin").mkdir()
        (root / "home").mkdir()
        native_id = str(uuid.uuid4())
        home, transcript = fixture(root, native_id)
        executor_home = root / "executor"
        controller_home = root / "controller"
        logical_id = "agit-" + str(uuid.uuid4())
        roster = executor_home / "desktop-rc" / "sessions.json"
        roster.parent.mkdir(parents=True)
        roster.write_text(json.dumps(dict(sessions={logical_id: dict(
            runtime="codex", thread_id=native_id, cwd=str(root / "project"),
            workspace_id="local-owner", project_id="test",
        )})))
        shim = root / "bin" / "ssh"
        shim.write_text("#!/usr/bin/env python3\nimport os, sys\nos.environ['AGIT_HOME'] = " + repr(str(executor_home)) + "\nos.execv('/bin/sh', ['sh', '-c', sys.argv[-1]])\n")
        shim.chmod(0o700)
        common = dict(PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                      HOME=str(root / "home"), CODEX_HOME=str(home), SHELL="/bin/sh")
        environments = [dict(common, AGIT_HOME=str(directory)) for directory in (executor_home, controller_home)]
        processes, natives, clients = [], [], []
        with (root / "fixture.log").open("w") as log:
            try:
                owner = await Native().start(common, log)
                natives.append(owner)
                params = dict(threadId=native_id, path=str(transcript), cwd=str(root / "project"),
                              model="fixture", modelProvider="fixture", approvalPolicy="never", sandbox="read-only")
                assert "result" in await owner.rpc("thread/resume", params)
                for env in environments:
                    processes.append(subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log))
                    endpoint = Path(env["AGIT_HOME"]) / "desktop-rc" / "control.rpc"

                    async def ready():
                        assert processes[-1].poll() is None, "fixture daemon exited"
                        return endpoint.exists()

                    await eventually(ready, "owner RPC did not become ready")
                    clients.append(await Client().connect(str(endpoint)))
                direct, controller = clients
                identity = (await direct.rpc("machine.describe"))["result"]["instance_id"]
                assert "result" in await direct.rpc("project.bind", project_id="test", local_path=str(root / "project"))
                connected = await controller.rpc("peer.connect", peer_id="executor", host="fixture", binary=binary)
                assert "result" in connected, connected
                route = connected["result"]

                async def peer(method, **params):
                    params.setdefault("workspace_id", "local-owner")
                    return await controller.rpc("peer.request", peer_id="executor", route_id=route["route_id"],
                                                generation=route["generation"], method=method, params=params)

                # Age is deliberately misleading; the native writer still owns its lock.
                os.utime(transcript, (1, 1))
                before = transcript.read_bytes()
                for call in (direct.rpc, peer, direct.rpc):
                    response = await call("session.resume", session_id=logical_id, prompt="Must not reach native input.")
                    assert response["error"]["code"] == 303, response
                    response = await call("session.enqueue", session_id=native_id,
                                          client_msg_id=str(uuid.uuid4()), message="Must not enter native inbox.")
                    assert "read-only" in response["error"]["message"], response
                    assert (await direct.rpc("session.list", include_local=False))["result"]["sessions"] == []
                assert transcript.read_bytes() == before, "rejected control changed the external transcript"
                assert owner.process.returncode is None, "Agit stopped the external owner"
                assert (await direct.rpc("machine.describe"))["result"]["instance_id"] == identity

                await owner.close()
                os.utime(transcript, None)
                listed = await direct.rpc("session.list", include_local=True)
                local = next(row for row in listed["result"]["local"] if row["runtime_session_id"] == native_id)
                assert not local["likely_active"], "released native writer must not remain read-only"
                resumed = await peer("session.resume", session_id=logical_id)
                assert "result" in resumed, resumed
                logical = resumed["result"]["session"]["session_id"]
                assert (await direct.rpc("session.resume", session_id=native_id))["result"]["session"]["session_id"] == logical
                contender = await Native().start(common, log)
                natives.append(contender)
                denied = await contender.rpc("thread/resume", params)
                assert "active writer" in denied["error"]["message"], denied
                assert (await direct.rpc("machine.describe"))["result"]["instance_id"] == identity
                print("PASS: direct/peer refusal, repeatable busy result, no inbox writes, release then resume, reverse native exclusion")
                print("Native version:", owner.version)
            except Exception:
                log.flush()
                print((root / "fixture.log").read_text()[-5000:])
                raise
            finally:
                for client in clients:
                    await client.close()
                for env in environments:
                    subprocess.run([binary, "rc", "local", "stop"], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                for process in processes:
                    try:
                        await asyncio.to_thread(process.wait, 10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        await asyncio.to_thread(process.wait)
                for native in natives:
                    await native.close()


if __name__ == "__main__":
    asyncio.run(run(str(Path(sys.argv[1]).resolve())))
