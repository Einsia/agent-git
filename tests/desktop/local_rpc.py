"""Exercise real local daemon transport without starting a model."""
import json
import os
from pathlib import Path
import socket
import selectors
import subprocess
import tempfile
import time


def run(binary):
    with tempfile.TemporaryDirectory(prefix="agd-", dir="/tmp") as directory:
        home = Path(directory)
        env = dict(os.environ, AGIT_HOME=directory)
        work = home / "project"
        work.mkdir()
        with (home / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
            clients = []
            try:
                endpoint = home / "desktop-rc" / "control.rpc"
                for _ in range(300):
                    if endpoint.exists():
                        break
                    if daemon.poll() is not None:
                        raise RuntimeError((home / "daemon.log").read_text())
                    time.sleep(0.05)
                for _ in range(2):
                    client = socket.socket(socket.AF_UNIX)
                    client.settimeout(30)
                    client.connect(str(endpoint))
                    clients.append((client, client.makefile("rb")))

                def send(client, method, params):
                    client[0].sendall((json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params)) + "\n").encode())

                def read(client):
                    response = json.loads(client[1].readline())
                    assert response["id"] == 1, response
                    return response

                for client in clients:
                    send(client, "machine.describe", {})
                descriptions = [read(client)["result"] for client in clients]
                assert descriptions[0]["instance_id"] == descriptions[1]["instance_id"]
                assert descriptions[0]["authority"] == "local-owner"
                bridge = subprocess.Popen([binary, "rc", "local", "bridge"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    bridge.stdin.write((json.dumps(dict(jsonrpc="2.0", id=41, method="machine.describe", params={})) + "\n").encode())
                    bridge.stdin.flush()
                    selector = selectors.DefaultSelector()
                    selector.register(bridge.stdout, selectors.EVENT_READ)
                    record = b""
                    deadline = time.monotonic() + 10
                    while b"\n" not in record:
                        assert selector.select(max(0, deadline - time.monotonic())), "Bridge did not flush a response while stdin remained open"
                        chunk = os.read(bridge.stdout.fileno(), 8192)
                        assert chunk, "Bridge closed before returning a response"
                        record += chunk
                    reply = json.loads(record.split(b"\n", 1)[0])
                    assert reply["id"] == 41
                    assert reply["result"]["instance_id"] == descriptions[0]["instance_id"]
                finally:
                    bridge.terminate()
                    bridge.wait(timeout=5)
                    selector.close()
                send(clients[0], "project.bind", dict(workspace_id="local-owner", project_id="test", local_path=str(work)))
                send(clients[1], "workspace.list", dict(workspace_id="hub-workspace"))
                assert "result" in read(clients[0])
                assert "error" in read(clients[1])
                send(clients[1], "workspace.list", {})
                assert read(clients[1])["result"]["workspaces"][0]["projects"][0]["local_path"] == str(work.resolve())
                send(clients[0], "session.list", dict(workspace_id="local-owner", include_local=False))
                assert read(clients[0])["result"]["sessions"] == []
                assert not (home / "rc").exists(), "Local commands wrote Hub control state"
                assert endpoint.stat().st_mode & 0o777 == 0o600
                print("PASS: real daemon, peer routing, scope rejection, binding, read-only discovery, private namespace")
            finally:
                for sock, file in clients:
                    file.close()
                    sock.close()
                subprocess.run([binary, "rc", "local", "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                try:
                    daemon.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    import sys
    run(str(Path(sys.argv[1]).resolve()))
