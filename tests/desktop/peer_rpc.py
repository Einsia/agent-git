"""Exercise daemon-owned peers and worker recovery using isolated process trees."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


class Client:
    def __init__(self, endpoint):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(50)
        self.socket.connect(str(endpoint))
        self.file = self.socket.makefile("rb")
        self.serial = 0
        self.events = []
        self.target = None

    def send(self, method, params):
        if method == "peer.request" and self.target:
            params = dict(params, **self.target)
        self.serial += 1
        self.socket.sendall((json.dumps(dict(jsonrpc="2.0", id=self.serial, method=method, params=params)) + "\n").encode())
        return self.serial

    def receive(self, identity):
        while True:
            raw = self.file.readline()
            assert raw, "Local daemon closed the client"
            frame = json.loads(raw)
            if frame.get("id") == identity:
                return frame
            self.events.append(frame)

    def request(self, method, params):
        frame = self.receive(self.send(method, params))
        assert "error" not in frame, frame
        if method == "peer.connect":
            self.target = {key: frame["result"][key] for key in ("route_id", "generation")}
        return frame["result"]

    def close(self):
        self.file.close()
        self.socket.close()


def wait_for(predicate, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("Condition did not become ready")


def run(binary):
    with tempfile.TemporaryDirectory(prefix="ag-peer-", dir="/tmp") as directory:
        root = Path(directory)
        controller_home = root / "controller"
        executor_home = root / "executor"
        shim = root / "ssh"
        shim.write_text("#!/usr/bin/env python3\nimport os, sys\nos.environ['AGIT_HOME'] = " + repr(str(executor_home)) + "\nos.execv('/bin/sh', ['sh', '-c', sys.argv[-1]])\n")
        shim.chmod(0o700)
        env = dict(os.environ, AGIT_HOME=str(controller_home), PATH=str(root) + os.pathsep + os.environ["PATH"])
        remote_env = dict(os.environ, AGIT_HOME=str(executor_home))
        log = (root / "daemon.log").open("w")
        daemon = subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
        clients = []
        try:
            endpoint = controller_home / "desktop-rc" / "control.rpc"
            wait_for(endpoint.exists)
            client = Client(endpoint)
            clients.append(client)
            local = client.request("machine.describe", {})
            config = dict(peer_id="remote", host="isolated-test", binary=binary)
            connection = client.request("peer.connect", config)
            remote = connection["description"]
            assert remote["instance_id"] != local["instance_id"]
            peer = client.request("peer.list", {})["peers"][0]
            assert peer["state"] == "online"
            assert peer["worker_pid"] != daemon.pid
            first_worker = peer["worker_pid"]
            remote_pid = int((executor_home / "desktop-rc" / "agitd.pid").read_text())
            assert remote_pid not in (first_worker, daemon.pid)
            restart = subprocess.run([binary, "rc", "local", "restart", "--if-idle"], env=env,
                                     capture_output=True, text=True, timeout=10)
            assert restart.returncode != 0 and json.loads(restart.stdout)["status"] == "deferred"
            assert client.request("machine.describe", {})["instance_id"] == local["instance_id"]
            assert client.request("peer.list", {})["peers"][0]["worker_pid"] == first_worker
            # Closing the UI leaves both the tunnel and executor under daemon ownership.
            client.close()
            clients.remove(client)
            client = Client(endpoint)
            clients.append(client)
            assert client.request("peer.connect", config)["generation"] == 1
            assert client.request("peer.list", {})["peers"][0]["worker_pid"] == first_worker
            # Concurrent local and remote requests use independent identity maps.
            ids = [client.send("peer.request", dict(peer_id="remote", method="machine.describe", params={})),
                   client.send("machine.describe", {})]
            replies = {}
            while len(replies) < 2:
                frame = json.loads(client.file.readline())
                if "id" in frame:
                    replies[frame["id"]] = frame
            assert replies[ids[0]]["result"]["instance_id"] == remote["instance_id"]
            assert replies[ids[1]]["result"]["instance_id"] == local["instance_id"]
            os.kill(first_worker, signal.SIGKILL)
            assert client.request("machine.describe", {})["instance_id"] == local["instance_id"]
            def recovered():
                peers = client.request("peer.list", {})["peers"]
                return peers[0] if peers[0]["state"] == "online" and peers[0]["generation"] > 1 else None
            restored = wait_for(recovered)
            assert restored["worker_pid"] != first_worker
            assert restored["description"]["instance_id"] == remote["instance_id"]
            assert daemon.poll() is None
            os.kill(remote_pid, 0)
            stale = client.receive(client.send("peer.request", dict(peer_id="remote", method="turn.start", params={})))
            assert stale["error"]["data"]["outcome"] == "not_sent", stale
            client.request("peer.connect", config)
            project = root / "project"
            project.mkdir()
            client.request("peer.request", dict(peer_id="remote", method="project.bind", params=dict(workspace_id="local-owner", project_id="test", local_path=str(project))))
            assert client.request("peer.request", dict(peer_id="remote", method="session.list", params=dict(include_local=False)))["sessions"] == []
            # One peer's explicit removal leaves local execution reachable.
            client.request("peer.disconnect", dict(peer_id="remote"))
            assert client.request("peer.list", {})["peers"] == []
            assert client.request("machine.describe", {})["instance_id"] == local["instance_id"]
            diagnostics = Path(local["diagnostic_log"])
            def logged():
                text = diagnostics.read_text()
                records = [json.loads(line) for line in text[:text.rfind("\n") + 1].splitlines() if line]
                return records if any(item["event"] == "rpc.completed" and item["metadata"].get("outcome") == "not_sent" for item in records) else None
            records = wait_for(logged)
            assert diagnostics.stat().st_mode & 0o777 == 0o600
            assert all(item["instance_id"] == local["instance_id"] for item in records)
            assert any(item["event"] == "peer.state" and item["metadata"].get("worker_pid") == restored["worker_pid"] for item in records)
            assert any(item["event"] == "rpc.completed" and item["metadata"].get("operation_id") for item in records)
            print("PASS: distinct daemons, daemon-owned tunnel, UI detach, response correlation, worker recovery, executor continuity")
            print("PASS: private diagnostics retain peer generations, worker identities, and failed operation correlation")
        finally:
            for client in clients:
                client.close()
            for environment in (env, remote_env):
                subprocess.run([binary, "rc", "local", "stop"], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
            try:
                daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            log.close()


if __name__ == "__main__":
    import sys
    run(str(Path(sys.argv[1]).resolve()))
