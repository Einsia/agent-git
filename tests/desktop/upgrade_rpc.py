"""Exercise safe local daemon replacement using isolated state and processes."""
import json
import base64
import hashlib
import io
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from peer_rpc import Client, wait_for
from launch_rpc import HARNESS


class Bridge:
    def __init__(self, binary, env, *flags):
        self.process = subprocess.Popen([binary, "rc", "local", "bridge", "--ensure", *flags], env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.process.stdin.write(b'{"jsonrpc":"2.0","id":41,"method":"machine.describe","params":{}}\n')
        self.process.stdin.flush()

    def description(self):
        assert select.select([self.process.stdout], [], [], 40)[0], "Bridge did not become ready"
        line = self.process.stdout.readline()
        assert line, self.process.stderr.read().decode()
        response = json.loads(line)
        assert response["id"] == 41, response
        return response["result"]

    def close(self):
        self.process.terminate()
        self.process.wait(timeout=5)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            stream.close()


class Release:
    def __init__(self, binary):
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as bundle:
            bundle.add(binary, arcname="package/bin/agit")
        payload = archive.getvalue()
        integrity = "sha512-" + base64.b64encode(hashlib.sha512(payload).digest()).decode()

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                base = f"http://127.0.0.1:{self.server.server_port}"
                if self.path == "/api/cli/version":
                    body = json.dumps(dict(version=self.server.release_version, tag="v" + self.server.release_version, url=base, repo="fixture/agit",
                                           stale=False, npm_package="@fixture/agit")).encode()
                elif self.path == "/fixture.tgz":
                    body = payload
                else:
                    body = json.dumps(dict(dist=dict(tarball=base + "/fixture.tgz", integrity=integrity))).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.release_version = "999.0.0"
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def install(source, destination):
    staged = destination.with_suffix(".new")
    shutil.copy2(source, staged)
    staged.replace(destination)


def control(home, **request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(home / "desktop-rc" / "control.sock"))
        connection.sendall((json.dumps(request) + "\n").encode())
        with connection.makefile("rb") as source:
            return json.loads(source.readline())


def stop_if_idle(home, identity):
    return control(home, op="stop_if_idle", instance_id=identity["instance_id"], build_id=identity["build_id"])


def has_stopped(home):
    try:
        control(home, op="status")
        return False
    except (FileNotFoundError, ConnectionRefusedError):
        return not (home / "desktop-rc" / "agitd.pid").exists()
    except (json.JSONDecodeError, ConnectionResetError, socket.timeout):
        return False


def run(binary):
    with tempfile.TemporaryDirectory(prefix="ag-up-", dir="/tmp") as directory:
        root = Path(directory)
        home = root / "state"
        user = root / "user"
        user.mkdir()
        project = root / "project"
        project.mkdir()
        fixtures = root / "bin"
        fixtures.mkdir()
        harness = fixtures / "codex"
        harness.write_text(HARNESS)
        harness.chmod(0o700)
        trace = root / "launches.jsonl"
        env = dict(os.environ, AGIT_HOME=str(home), HOME=str(user), SHELL="/bin/sh", AGIT_TELEMETRY_DEFER="1",
                   AGIT_INTERNAL_UPDATE_RESTART="1", CODEX_HOME=str(root / "codex"), AGIT_TEST_LAUNCH_LOG=str(trace),
                   PATH=str(fixtures) + os.pathsep + os.environ["PATH"])
        with (root / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
            client = None
            try:
                endpoint = home / "desktop-rc" / "control.rpc"
                wait_for(endpoint.exists)
                client = Client(endpoint)
                identity = control(home, op="status")["identity"]
                assert stop_if_idle(home, dict(identity, instance_id="obsolete"))["reply"] == "instance_changed"
                client.request("project.bind", dict(workspace_id="local-owner", project_id="fixture", local_path=str(project)))
                start = dict(workspace_id="local-owner", project_id="fixture", runtime="codex", start_id=str(uuid.uuid4()))
                session = client.request("session.start", start)

                def spawned():
                    return [item for line in trace.read_text().splitlines()
                            if (item := json.loads(line))["event"] == "thread"] if trace.exists() else []

                native = wait_for(spawned)[0]
                assert stop_if_idle(home, identity)["reply"] == "busy"
                assert control(home, op="status")["identity"] == identity
                os.kill(native["pid"], 0)
                assert len(spawned()) == 1
                os.kill(native["pid"], signal.SIGTERM)
                wait_for(lambda: not client.request("session.list", dict(workspace_id="local-owner", include_local=False))["sessions"])
                terminal = client.request("terminal.open", dict(workspace_id="local-owner", project_id="fixture"))
                reply = stop_if_idle(home, identity)
                assert reply["reply"] == "busy", reply
                assert daemon.poll() is None
                assert client.request("machine.describe", {})["instance_id"] == identity["instance_id"]
                client.request("terminal.close", dict(terminal_id=terminal["terminal_id"]))

                def stopped():
                    response = stop_if_idle(home, identity)
                    assert response["reply"] in ("busy", "stopping"), response
                    return response["reply"] == "stopping"

                wait_for(stopped)
                daemon.wait(timeout=15)
                assert daemon.returncode == 0
                assert (home / "desktop-rc" / "workspaces.json").exists()
                resumed = Bridge(binary, env)
                try:
                    assert resumed.description()["instance_id"] != identity["instance_id"]
                    replay = Client(home / "desktop-rc" / "control.rpc")
                    try:
                        assert replay.request("session.start", start) == session
                        assert len(spawned()) == 1
                    finally:
                        replay.close()
                finally:
                    resumed.close()
                    subprocess.run([binary, "rc", "local", "stop"], env=env, capture_output=True, timeout=5)
                    wait_for(lambda: has_stopped(home))
                print("PASS: instance fence, live session and terminal preservation, safe idle stop, durable start replay")
            finally:
                if client:
                    client.close()
                if daemon.poll() is None:
                    subprocess.run([binary, "rc", "local", "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                    try:
                        daemon.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        daemon.kill()
                        daemon.wait()


def replacement_checks(binary, old_binary, legacy_binary):
    assert subprocess.check_output([binary, "--version"]) == subprocess.check_output([old_binary, "--version"])
    release = Release(binary)
    try:
        with tempfile.TemporaryDirectory(prefix="ag-replace-", dir="/tmp") as directory:
            root = Path(directory)
            home = root / "state"
            unrelated = root / "unrelated"
            user = root / "user"
            user.mkdir()
            project = root / "project"
            project.mkdir()
            installed = root / "agit"
            install(old_binary, installed)
            env = dict(os.environ, AGIT_HOME=str(home), HOME=str(user), SHELL="/bin/sh",
                       AGIT_TELEMETRY_DEFER="1", AGIT_INTERNAL_UPDATE_RESTART="1",
                       AGIT_HUB_URL=release.url, AGIT_NPM_REGISTRY=release.url)
            other_env = dict(env, AGIT_HOME=str(unrelated))
            processes, bridges, clients = [], [], []
            log = (root / "daemon.log").open("w")

            def start(executable, environment):
                process = subprocess.Popen([executable, "rc", "local", "start"], env=environment, stdout=log, stderr=log)
                processes.append(process)
                endpoint = Path(environment["AGIT_HOME"]) / "desktop-rc" / "control.rpc"
                def connected():
                    if process.poll() is not None:
                        raise AssertionError((root / "daemon.log").read_text())
                    try:
                        return Client(endpoint)
                    except (FileNotFoundError, ConnectionRefusedError):
                        return None
                client = wait_for(connected)
                clients.append(client)
                client.request("machine.describe", {})
                return client

            def upgrade(environment):
                result = subprocess.run([str(installed), "upgrade"], env=environment, capture_output=True, text=True, timeout=60)
                assert result.returncode == 0, result.stdout + result.stderr
                return result

            try:
                old = start(str(installed), env)
                other = start(binary, other_env)
                other_identity = other.request("machine.describe", {})["instance_id"]
                before = old.request("machine.describe", {})
                old.request("project.bind", dict(workspace_id="local-owner", project_id="fixture", local_path=str(project)))
                # The installed executable can differ from the image already serving the socket.
                install(binary, installed)
                checked = subprocess.run([str(installed), "upgrade", "--check"], env=env, capture_output=True, timeout=30)
                assert checked.returncode == 0
                assert old.request("machine.describe", {})["instance_id"] == before["instance_id"]
                result = upgrade(env)
                assert "restarted" in result.stderr.lower(), result.stdout + result.stderr
                current = Client(home / "desktop-rc" / "control.rpc")
                clients.append(current)
                after = current.request("machine.describe", {})
                assert after["instance_id"] != before["instance_id"]
                assert after["build_id"] != before["build_id"]
                assert current.request("peer.list", {})["peers"] == []
                projects = current.request("workspace.list", {})["workspaces"][0]["projects"]
                assert projects[0]["local_path"] == str(project.resolve())
                assert other.request("machine.describe", {})["instance_id"] == other_identity

                invalid_env = dict(env, AGIT_HOME=str(root / "unsupported"))
                invalid = subprocess.run([binary, "rc", "local", "bridge", "--ensure", "--require-feature", "unsupported-fixture"],
                                         env=invalid_env, input=b"", capture_output=True, timeout=10)
                assert invalid.returncode != 0 and not invalid.stdout
                assert not (root / "unsupported" / "desktop-rc" / "agitd.pid").exists()

                absent_env = dict(env, AGIT_HOME=str(root / "absent"))
                upgrade(absent_env)
                assert not (root / "absent" / "desktop-rc" / "agitd.pid").exists()

                subprocess.run([binary, "rc", "local", "stop"], env=env, check=True, capture_output=True)
                wait_for(lambda: has_stopped(home))
                install(old_binary, installed)
                busy = start(str(installed), env)
                before = busy.request("machine.describe", {})
                terminal = busy.request("terminal.open", dict(workspace_id="local-owner", project_id="fixture"))
                install(binary, installed)
                result = upgrade(env)
                assert "deferred" in result.stderr.lower(), result.stdout + result.stderr
                assert busy.request("machine.describe", {})["instance_id"] == before["instance_id"]
                blocked = subprocess.run([str(installed), "rc", "local", "bridge", "--ensure", "--require-current-build"],
                                         env=env, input=b"", capture_output=True, timeout=30)
                assert blocked.returncode != 0 and b"local daemon" in blocked.stderr
                assert str(home).encode() in blocked.stderr and b"restart --if-idle" in blocked.stderr
                assert not blocked.stdout
                busy.request("terminal.close", dict(terminal_id=terminal["terminal_id"]))
                def terminal_closed():
                    busy.request("workspace.list", {})
                    return any(event.get("method") == "terminal.exited" and
                               event.get("params", {}).get("terminal_id") == terminal["terminal_id"]
                               for event in busy.events)

                wait_for(terminal_closed)
                bridges.extend([Bridge(str(installed), env, "--require-current-build") for _ in range(2)])
                descriptions = [bridge.description() for bridge in bridges]
                assert descriptions[0]["instance_id"] == descriptions[1]["instance_id"]
                assert descriptions[0]["instance_id"] != before["instance_id"]
                for bridge in bridges:
                    bridge.close()
                bridges.clear()
                assert other.request("machine.describe", {})["instance_id"] == other_identity

                subprocess.run([binary, "rc", "local", "stop"], env=env, check=True, capture_output=True)
                wait_for(lambda: has_stopped(home))
                install(old_binary, installed)
                stale = start(str(installed), env)
                stale_identity = stale.request("machine.describe", {})["instance_id"]
                install(binary, installed)
                release.server.release_version = subprocess.check_output([binary, "--version"], text=True).split()[1]
                current_result = upgrade(env)
                assert "up to date" in current_result.stdout and "restarted" in current_result.stderr.lower()
                assert control(home, op="status")["identity"]["instance_id"] != stale_identity
                subprocess.run([binary, "rc", "local", "stop"], env=env, check=True, capture_output=True)
                wait_for(lambda: has_stopped(home))
                install(legacy_binary, installed)
                legacy = start(str(installed), env)
                legacy_identity = legacy.request("machine.describe", {})["instance_id"]
                install(binary, installed)
                legacy_result = subprocess.run([str(installed), "rc", "local", "bridge", "--ensure", "--require-current-build"],
                                              env=env, input=b"", capture_output=True, timeout=30)
                assert legacy_result.returncode != 0 and b"safe restart" in legacy_result.stderr
                assert not legacy_result.stdout
                assert legacy.request("machine.describe", {})["instance_id"] == legacy_identity
                print("PASS: real upgrade, same-version builds, state retention, busy deferral, concurrent bridges, legacy fallback, namespace isolation")
            finally:
                for bridge in bridges:
                    bridge.close()
                for client in clients:
                    client.close()
                for environment in (env, other_env):
                    subprocess.run([binary, "rc", "local", "stop"], env=environment, capture_output=True, timeout=5)
                for process in processes:
                    try:
                        process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                log.close()
    finally:
        release.close()


def startup_failure_check(binary, old_binary):
    with tempfile.TemporaryDirectory(prefix="ag-failed-up-", dir="/tmp") as directory:
        home = Path(directory)
        env = dict(os.environ, AGIT_HOME=directory, HOME=directory,
                   AGIT_TELEMETRY_DEFER="1", AGIT_INTERNAL_UPDATE_RESTART="1")
        with (home / "old.log").open("w") as log:
            daemon = subprocess.Popen([old_binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
            client = None
            try:
                endpoint = home / "desktop-rc" / "control.rpc"
                wait_for(endpoint.exists)
                client = Client(endpoint)
                client.request("machine.describe", {})
                project = home / "project"
                project.mkdir()
                client.request("project.bind", dict(workspace_id="local-owner", project_id="fixture", local_path=str(project)))
                bindings = home / "desktop-rc" / "workspaces.json"
                before = bindings.read_bytes()
                vault = home / "secret-filter" / "vault.json"
                vault.parent.mkdir(exist_ok=True)
                vault.write_text("invalid fixture vault\n")
                vault.chmod(0o600)
                result = subprocess.run([binary, "rc", "local", "restart", "--if-idle"], env=env,
                                        capture_output=True, text=True, timeout=40)
                assert result.returncode != 0 and "did not become ready" in result.stderr, result
                assert "agitd-*.log" in result.stderr and not result.stdout
                daemon.wait(timeout=5)
                assert daemon.returncode == 0
                assert has_stopped(home)
                assert bindings.read_bytes() == before
                assert vault.read_text() == "invalid fixture vault\n"
                logs = list((home / "desktop-rc").glob("agitd-*.log"))
                assert len(logs) == 1, "a failed replacement must not enter a spawn loop"
                assert logs[0].read_text().strip()
                print("PASS: failed replacement reports diagnostics, preserves state, and does not respawn repeatedly")
            finally:
                if client:
                    client.close()
                subprocess.run([binary, "rc", "local", "stop"], env=env, capture_output=True, timeout=5)
                if daemon.poll() is None:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    import sys
    run(str(Path(sys.argv[1]).resolve()))
    if len(sys.argv) == 4:
        replacement_checks(*(str(Path(argument).resolve()) for argument in sys.argv[1:]))
        startup_failure_check(*(str(Path(argument).resolve()) for argument in sys.argv[1:3]))
