"""Validate a deployed relay against an isolated remote daemon and real harness.

Read a JSON login object from stdin. SSH is used only for fixture setup, identity
inspection, diagnostics, and cleanup; session RPCs travel through the cloud peer.
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import uuid

from cloud_rpc import Client, command, eventually


def api(hub, path, token=None, data=None, method=None):
    config = ["url = " + json.dumps(hub + path), 'header = "Content-Type: application/json"']
    if token:
        config.append("header = " + json.dumps("Authorization: Bearer " + token))
    if data is not None:
        config.append("data = " + json.dumps(json.dumps(data)))
    result = subprocess.run(["curl", "--silent", "--show-error", "--max-time", "30",
        "--request", method or ("POST" if data is not None else "GET"),
        "--write-out", "\n%{http_code}", "--config", "-"],
        input="\n".join(config), capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"Cloud API {path} transport failed")
    body, status = result.stdout.rsplit("\n", 1)
    if not 200 <= int(status) < 300:
        raise RuntimeError(f"Cloud API {path} returned HTTP {status}")
    return json.loads(body) if body else None


async def remote(args, environment, *command_args, input=None):
    invocation = ["env", *(key + "=" + value for key, value in environment.items()), *command_args]
    process = await asyncio.create_subprocess_exec("ssh", "-o", "BatchMode=yes", args.host,
        shlex.join(invocation), stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    output, error = await asyncio.wait_for(process.communicate(input), 60)
    assert process.returncode == 0, error.decode()
    return output.decode()


async def owner_remote(args, environment, method, **params):
    request = dict(jsonrpc="2.0", id="fixture", method=method, params=params)
    invocation = ["env", *(key + "=" + value for key, value in environment.items()),
                  args.remote_binary, "rc", "local", "bridge"]
    process = await asyncio.create_subprocess_exec("ssh", "-o", "BatchMode=yes", args.host,
        shlex.join(invocation), stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
    try:
        process.stdin.write((json.dumps(request) + "\n").encode())
        await process.stdin.drain()
        while line := await asyncio.wait_for(process.stdout.readline(), 30):
            response = json.loads(line)
            if response.get("id") == "fixture":
                assert "error" not in response, response
                return response["result"]
        raise AssertionError("remote fixture owner bridge closed")
    finally:
        process.stdin.close()
        await asyncio.wait_for(process.wait(), 10)


async def run(args, login):
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix="agd-cloud-live-", dir="/tmp")).resolve()
    remote_root = args.remote_root.rstrip("/")
    environment = dict(os.environ, AGIT_HOME=str(root / "agit"), AGIT_HUB_URL=args.hub, SHELL="/bin/bash")
    for name in ("AGIT_SESSION", "AGIT_MERGE_TX", "AGIT_RC"):
        environment.pop(name, None)
    target_env = dict(AGIT_HOME=remote_root + "/agit", AGIT_HUB_URL=args.hub, SHELL="/bin/bash")
    response = await asyncio.to_thread(api, args.hub, "/api/auth/login", data=login)
    login.clear()
    token = response["access_token"]
    account = response["account_id"]
    del response
    devices, local_daemon, controller, remote_started = [], None, None, False
    source_login, target_login = False, False
    journal = (root / "events.jsonl").open("w")
    log = (root / "daemon.log").open("w")
    print("Live cloud evidence:", root, flush=True)
    try:
        await remote(args, {}, "mkdir", "-p", remote_root + "/project")
        # A live test must never attach to or replace an existing daemon namespace.
        await remote(args, {}, "test", "!", "-e", remote_root + "/agit/desktop-rc/control.rpc")
        await remote(args, target_env, args.remote_binary, "login", "--hub", args.hub, "--with-token", input=token.encode())
        target_login = True
        await remote(args, target_env, args.remote_binary, "rc", "start", "--detach")
        remote_started = True
        async def target_ready():
            try:
                return await owner_remote(args, target_env, "machine.describe")
            except AssertionError:
                return None
        target = await eventually(target_ready, "remote owner daemon did not become ready")
        await command(args.binary, environment, "login", "--hub", args.hub, "--with-token", input=token.encode())
        source_login = True
        local_daemon = await asyncio.create_subprocess_exec(args.binary, "rc", "local", "start", env=environment, stdout=log, stderr=log)
        async def source_ready():
            assert local_daemon.returncode is None, "local daemon exited"
            return (root / "agit/desktop-rc/control.rpc").exists()
        await eventually(source_ready, "local controller did not become ready")
        controller = await Client().connect(args.binary, environment, journal)
        target_device = None
        async def online():
            nonlocal target_device
            page = await controller.rpc("peer.cloud", operation="devices", hub=args.hub)
            if not page["devices"]:
                return False
            target_device = page["devices"][0]["device"]
            return page["devices"][0]["online"]
        await eventually(online, "remote outbound presence did not reach dev", timeout=60)
        assert target_device is not None
        config = dict(peer_id="target", hub=args.hub, target=target_device)
        connected = await controller.rpc("peer.connect_cloud", **config)
        assert connected["description"]["instance_id"] == target["instance_id"]
        assert connected["description"]["authority"] == "cloud-principal"
        await controller.peer("project.bind", project_id="cloud-live", local_path=remote_root + "/project")
        await controller.peer("workspace.list")
        start = dict(project_id="cloud-live", runtime=args.runtime, start_id=str(uuid.uuid4()))
        if args.model:
            start["model"] = args.model
        opened = await controller.peer("session.start", **start)
        session = opened["session"]["session_id"]
        assert await controller.peer("session.start", **start) == opened
        await controller.peer("session.subscribe", session_id=session, after_seq=0)
        marker = "CLOUD_LIVE_" + uuid.uuid4().hex
        message = dict(session_id=session, client_msg_id=str(uuid.uuid4()),
                       message=f"Reply with exactly {marker}. Do not use tools or edit any files.")
        receipt = await controller.peer("turn.start", **message)
        assert await controller.peer("turn.start", **message) == receipt
        async def answered():
            complete = controller.events(session, "turn.completed")
            if not complete:
                return None
            assert complete[-1]["params"].get("outcome") == "ok", complete[-1]["params"]
            history = await controller.peer("session.history", session_id=session)
            replies = [item.get("event", {}) for item in history.get("items", [])]
            matched = any(event.get("kind") == "assistant_reply" and marker in (event.get("text") or "") for event in replies)
            return history if complete and matched else None
        await eventually(answered, "real harness reply did not reach cloud history", timeout=300)
        print("PASS: dev relay delivered a real harness turn, duplicate receipt, events and history", flush=True)
        first = (await controller.rpc("peer.list"))["peers"][0]
        os.kill(first["worker_pid"], signal.SIGKILL)
        async def recovered():
            peer = (await controller.rpc("peer.list"))["peers"][0]
            return peer if peer["state"] == "online" and peer["generation"] > first["generation"] else None
        restored = await eventually(recovered, "cloud worker did not reconnect", timeout=60)
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
        await eventually(replayed, "remote session replay was lost")
        assert await controller.peer("turn.start", **message) == receipt
        before = len(controller.events(session, "turn.completed"))
        await controller.peer("turn.start", session_id=session, client_msg_id=str(uuid.uuid4()),
                              message="Repeat the exact marker from your previous reply and append _RECONNECTED. Do not use tools or edit files.")
        async def continued():
            complete = controller.events(session, "turn.completed")
            if len(complete) <= before:
                return False
            assert complete[-1]["params"].get("outcome") == "ok", complete[-1]["params"]
            history = await controller.peer("session.history", session_id=session)
            return any(item.get("event", {}).get("kind") == "assistant_reply"
                       and marker + "_RECONNECTED" in (item["event"].get("text") or "")
                       for item in history.get("items", []))
        await eventually(continued, "real conversation context did not survive reconnect", timeout=300)
        if args.desktop_test:
            desktop_env = dict(environment, AGD_TEST_CLOUD_SESSION=session, AGD_TEST_CLOUD_MARKER=marker + "_RECONNECTED",
                AGD_TEST_CLOUD_SEND_MARKER=marker + "_DESKTOP",
                AGD_TEST_CLOUD_MACHINE=json.dumps(dict(id="desktop-cloud-live", name="Cloud live executor", kind="cloud",
                    binary="@bundled", cloud=dict(hub=args.hub, target=target_device))))
            desktop = await asyncio.create_subprocess_exec(args.desktop_test, "cloud_bridge_ipc", "--ignored", "--nocapture", env=desktop_env)
            assert await asyncio.wait_for(desktop.wait(), 300) == 0, "Desktop IPC failed against the real executor"
            print("PASS: Desktop IPC submitted a real harness turn and received its reply", flush=True)
        await remote(args, target_env, args.remote_binary, "rc", "cloud", "grant", "--hub", args.hub,
                     "--account", account, "--resource", "session:" + session, "--access", "deny")
        async def denied():
            reply = await controller.raw("peer.request", peer_id="target", **controller.route,
                                         method="session.history", params=dict(session_id=session))
            if error := reply.get("error"):
                assert error["code"] == 201, reply
                return True
            return False
        await eventually(denied, "executor session denial did not take effect")
        catalog = await controller.peer("session.list", include_local=False)
        assert not any(row["session_id"] == session for row in catalog["sessions"])
        assert (await owner_remote(args, target_env, "machine.describe"))["instance_id"] == target["instance_id"]
        print("PASS: worker replacement, executor continuity, replay, stable receipt and executor denial", flush=True)
        summary = dict(hub=args.hub, runtime=args.runtime, instance_id=target["instance_id"],
                       session_id=session, source_generation=first["generation"], recovered_generation=restored["generation"],
                       remote_evidence=remote_root, real_harness=True)
        (root / "result.json").write_text(json.dumps(summary, indent=2))
    finally:
        if controller:
            await controller.close()
        if local_daemon:
            try:
                await command(args.binary, environment, "rc", "local", "stop")
                await asyncio.wait_for(local_daemon.wait(), 30)
            except (AssertionError, asyncio.TimeoutError):
                local_daemon.kill()
                await local_daemon.wait()
        if remote_started:
            await remote(args, target_env, args.remote_binary, "rc", "local", "stop")
        for device in devices:
            await asyncio.to_thread(api, args.hub, "/api/peer/devices/" + device, token=token, method="DELETE")
        if source_login:
            await command(args.binary, environment, "logout")
        if target_login:
            await remote(args, target_env, args.remote_binary, "logout")
        await asyncio.to_thread(api, args.hub, "/api/auth/logout", token=token, method="POST")
        journal.close()
        log.close()
        print("Retained private diagnostics:", root, "and", remote_root, flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--remote-binary", required=True)
    parser.add_argument("--remote-root", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--hub", required=True)
    parser.add_argument("--runtime", default="codex")
    parser.add_argument("--model")
    parser.add_argument("--desktop-test")
    asyncio.run(run(parser.parse_args(), json.loads(sys.stdin.readline())))
