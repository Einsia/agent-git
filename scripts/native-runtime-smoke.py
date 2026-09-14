#!/usr/bin/env python3
"""Exercise installed runtimes with isolated profiles and a local synthetic model."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(argv, env, cwd, timeout=120):
    result = subprocess.run(argv, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} exited {result.returncode}: {result.stderr[-2000:]}")
    return result.stdout


def exercise(runtime, exe, bridge, repository, source_runtime=None):
    root = Path(tempfile.mkdtemp(prefix=f"agit-{runtime}-smoke-")).resolve()
    workspace = root / "workspace"
    workspace.mkdir()
    default_workspace = root / "default-workspace"
    default_workspace.mkdir()
    (workspace / "runtime-cwd.txt").write_text("TARGET_WORKSPACE_671\n")
    (default_workspace / "runtime-cwd.txt").write_text("WRONG_WORKSPACE_671\n")
    env = {key: os.environ[key] for key in ("PATH", "HOME", "USER", "TMPDIR", "LANG", "SHELL") if key in os.environ}
    env.update({
        "AGIT_HOME": str(root / "agit"), "AGIT_HUB_URL": "https://native-runtime.invalid",
        "AGIT_SECRETS_KEYSTORE": "file", "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
    })
    (root / "gitconfig").write_text('[user]\nname = Native runtime test\nemail = runtime@example.invalid\n[commit]\ngpgsign = false\n')
    requests = []

    class Model(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(body)
            common = {"id": "chatcmpl-native-smoke", "created": 1, "model": body.get("model", "probe-model")}
            usage = {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}
            reply = {"role": "assistant", "content": "RESTORE_CONFIRMED_671"}
            finish = "stop"
            if runtime == "openclaw" and body["messages"][-1]["role"] != "tool":
                reply = {"role": "assistant", "tool_calls": [{"id": f"cwd-call-{len(requests)}", "type": "function", "function": {"name": "read", "arguments": json.dumps({"path": "runtime-cwd.txt"})}}]}
                finish = "tool_calls"
            if body.get("stream"):
                common["object"] = "chat.completion.chunk"
                delta = dict(reply)
                if "tool_calls" in delta:
                    delta["tool_calls"] = [{"index": index, **call} for index, call in enumerate(delta["tool_calls"])]
                chunks = [
                    {**common, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                    {**common, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage},
                ]
                payload = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                payload = json.dumps({**common, "object": "chat.completion", "choices": [{"index": 0, "message": reply, "finish_reason": finish}], "usage": usage}).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}"
    native = shutil.which("workbuddy-cli" if runtime == "workbuddy" else runtime)
    if not native:
        server.shutdown()
        raise RuntimeError(f"Install {runtime} and expose its CLI on PATH")
    try:
        if runtime == "workbuddy":
            env.update({"WORKBUDDY_CONFIG_DIR": str(root), "CODEBUDDY_CONFIG_DIR": str(root)})
            config = {"models": [{"id": "probe-model", "name": "probe-model", "url": base + "/chat/completions", "apiKey": "synthetic-probe-key", "trustLevel": "custom", "tags": ["custom"], "supportsToolCall": True, "supportsImages": False, "supportsReasoning": False, "maxInputTokens": 65536, "maxOutputTokens": 128}], "availableModels": ["probe-model"]}
            (root / "models.json").write_text(json.dumps(config))
        elif runtime == "hermes":
            env.update({"HERMES_HOME": str(root), "HERMES_PROBE_KEY": "synthetic-probe-key"})
            config = {"model": {"default": "probe-model", "provider": "probe"}, "providers": {"probe": {"base_url": base + "/v1", "key_env": "HERMES_PROBE_KEY", "default_model": "probe-model", "transport": "chat_completions"}}, "agent": {"max_turns": 1}, "compression": {"enabled": False}, "terminal": {"backend": "local", "cwd": str(workspace)}}
            (root / "config.yaml").write_text(json.dumps(config))
        else:
            env["OPENCLAW_STATE_DIR"] = str(root)
            config = {"gateway": {"mode": "local"}, "agents": {"defaults": {"workspace": str(default_workspace), "skipBootstrap": True, "model": {"primary": "probe/probe-model"}}}, "tools": {"profile": "coding", "allow": ["read"]}, "models": {"providers": {"probe": {"baseUrl": base + "/v1", "apiKey": "synthetic-probe-key", "api": "openai-completions", "models": [{"id": "probe-model", "name": "probe-model", "reasoning": False, "input": ["text"], "contextWindow": 65536, "maxTokens": 128}]}}}}
            (root / "openclaw.json").write_text(json.dumps(config))

        rows = [
            {"type": "user", "sessionId": "source-session", "cwd": str(workspace), "message": {"role": "user", "content": "Remember ARCHIVE_MARKER_671 and read probe.txt."}},
            {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "call-1", "name": "Read", "input": {"file_path": "probe.txt"}}]}},
            {"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": "TOOL_MARKER_671"}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "The marker is saved."}]}},
        ]
        source = root / "source.jsonl"
        source.write_text("".join(json.dumps(row) + "\n" for row in rows))
        path, sid, _ = run([bridge, "claude-code", str(source), runtime, str(workspace)], env, workspace).strip().splitlines()
        before = Path(path).read_bytes()
        clone_path, clone_id, _ = run([bridge, runtime, path, runtime, str(workspace)], env, workspace).strip().splitlines()
        assert clone_id != sid, "same-runtime restoration reused the source identity"
        if runtime == "hermes":
            with sqlite3.connect(root / "state.db") as connection:
                connection.row_factory = sqlite3.Row
                def messages(identity):
                    return [{key: value for key, value in dict(row).items() if key not in ("id", "session_id", "display_identity")} for row in connection.execute("SELECT * FROM messages WHERE session_id=? ORDER BY id", (identity,))]
                assert messages(sid) == messages(clone_id), "same-runtime restoration changed native fields"
            helper = (repository / "src/adapter/hermes/install.py").read_text()
            python = str(Path(native).resolve().parent / "python")
            collision = subprocess.run([python, "-c", helper], input=json.dumps({"content": Path(path).read_text(), "id": sid, "cwd": str(workspace)}), env=env, cwd=workspace, capture_output=True, text=True, timeout=30)
            assert collision.returncode and "refusing to overwrite" in collision.stderr
        elif runtime == "openclaw":
            original = [json.loads(line) for line in before.splitlines()]
            original[0]["id"] = clone_id
            assert original == [json.loads(line) for line in Path(clone_path).read_bytes().splitlines()]

        host = b"native-runtime.invalid"
        authority = "v2~" + hashlib.sha256(b"agit.hub-authority.v2\0" + len(host).to_bytes(8, "big") + host + b"\0").hexdigest()
        credentials = root / "agit/credentials"
        credentials.mkdir(parents=True, exist_ok=True)
        (credentials / (authority + ".json")).write_text(json.dumps({"username": "local", "email": "runtime@example.invalid", "hub": env["AGIT_HUB_URL"], "access_token": "synthetic-local-test", "access_expires_at": "2099-01-01T00:00:00Z", "refresh_token": "synthetic-local-test", "refresh_expires_at": "2099-01-01T00:00:00Z"}))

        def agit(args):
            output = run([exe, *args], env, workspace)
            (root / ("agit-" + args[0] + ".log")).write_text(output)
            return output

        setup = ["setup", "--runtime", runtime, "--hooks", "--skill"]
        if runtime != "openclaw":
            setup.append("--mcp")
        agit(setup)
        agit(["init", "native-capture", "--no-bind", "--auto-push=false", "--json"])
        target = "local/native-capture@" + runtime
        source_runtime = source_runtime or runtime
        source_id = sid
        if source_runtime != runtime:
            source_root = root / "source-runtime"
            source_root.mkdir()
            if source_runtime == "hermes":
                env["HERMES_HOME"] = str(source_root)
            elif source_runtime == "workbuddy":
                env["WORKBUDDY_CONFIG_DIR"] = str(source_root)
                env["CODEBUDDY_CONFIG_DIR"] = str(source_root)
            _, source_id, _ = run([bridge, "claude-code", str(source), source_runtime, str(workspace)], env, workspace).strip().splitlines()
        agit(["import", source_id, "--from", source_runtime, "--into", target, "--independent", "--json"])
        new_args = ["new", "local/native-capture", "--as", runtime, "-b", "public-new", "--no-launch", "--fresh"]
        launch = run([exe, *new_args, "--cwd", "workspace"], env, root) if runtime == "openclaw" else agit(new_args)
        assert "AGIT_SESSION='local/native-capture@public-new'" in launch
        assert ("workbuddy-cli" if runtime == "workbuddy" else runtime) in launch
        if runtime == "openclaw":
            resident = root / "resident-handle.mjs"
            resident.write_text('if (/\\/openclaw-[0-9a-f]+\\.mjs$/.test(process.argv[1] ?? "")) { process.stderr.write("SMOKE_RESIDENT_HANDLE\\n"); setInterval(() => {}, 60000); }\n')
            launch_env = {**env, "NODE_OPTIONS": "--import=" + str(resident)}
            launch_line = next(line.strip() for line in launch.splitlines() if line.strip().startswith("(export AGIT_SESSION="))
            result = subprocess.run(["sh", "-c", launch_line], input="Read runtime-cwd.txt.\nRead runtime-cwd.txt again.\n", env=launch_env, cwd=root, capture_output=True, text=True, timeout=120)
            (root / "new.stdout").write_text(result.stdout)
            (root / "new.stderr").write_text(result.stderr)
            (root / "new-requests.json").write_text(json.dumps(requests, indent=2))
            assert result.stderr.count("SMOKE_RESIDENT_HANDLE") == 2
            assert result.returncode == 0, result.stderr
            assert result.stdout.count("RESTORE_CONFIRMED_671") == 2, "native cleanup prevented the next prompt"
            assert "RESTORE_CONFIRMED_671" in agit(["show", "local/native-capture@public-new", "--json"])
            assert any(message.get("role") == "tool" and "TARGET_WORKSPACE_671" in json.dumps(message) for request in requests for message in request["messages"]), "new session read from the configured default workspace"
            requests.clear()
        exported = root / "public-export.jsonl"
        agit(["export", target, "--format", runtime, "-o", str(exported)])
        assert "ARCHIVE_MARKER_671" in exported.read_text() and "TOOL_MARKER_671" in exported.read_text()
        prepared = agit(["resume", target, "--as", runtime, "--no-launch", "--force"])
        match = re.search(r"--(?:resume|session-id) '([^']+)'", prepared)
        assert match, "public resume did not produce a native continuation command"
        sid = match.group(1)
        run([bridge, "--snapshot", runtime, sid, str(root / "prepared.jsonl")], env, workspace)
        before = (root / "prepared.jsonl").read_bytes()
        if runtime == "workbuddy":
            command = [native, "-p", "Recall the saved marker.", "--resume", sid, "--model", "probe-model", "--output-format", "json", "--max-turns", "1", "--system-prompt", "Follow the user request."]
        elif runtime == "hermes":
            command = [native, "chat", "--resume", sid, "--query", "Recall the saved marker.", "--oneshot", "--accept-hooks", "--provider", "probe", "--model", "probe-model", "--max-turns", "1", "--ignore-rules"]
        else:
            launch_line = next(line.strip() for line in prepared.splitlines() if "--session-id" in line and "--cwd" in line)
            bridge_command = re.search(r"'([^']+/node)' '([^']+\.mjs)' --root '([^']+)'", launch_line)
            assert bridge_command, "public resume did not select the workspace-bound OpenClaw launcher"
            command = [*bridge_command.groups()[:2], "--root", bridge_command.group(3), "--session-id", sid, "--cwd", str(workspace), "--message", "Recall the saved marker.", "--json", "--timeout", "60"]
        if runtime == "openclaw":
            failed = subprocess.run([*command, "--agent", "missing-smoke-agent"], env=launch_env, cwd=workspace, capture_output=True, text=True, timeout=30)
            (root / "failed-launch.stderr").write_text(failed.stderr)
            assert "SMOKE_RESIDENT_HANDLE" in failed.stderr
            assert failed.returncode != 0 and "missing-smoke-agent" in failed.stderr, "native launch did not report its error"
        output = run(command, launch_env if runtime == "openclaw" else env, workspace)
        (root / "native.stdout").write_text(output)
        (root / "requests.json").write_text(json.dumps(requests, indent=2))
        observed = [request for request in requests if request.get("stream")] if runtime == "hermes" else requests
        assert observed, "native model request was not observed"
        messages = json.dumps(observed[0]["messages"])
        assert "ARCHIVE_MARKER_671" in messages and "TOOL_MARKER_671" in messages
        assert any(message.get("role") == "tool" for message in observed[0]["messages"])
        assert "RESTORE_CONFIRMED_671" in output
        if runtime == "openclaw":
            assert any(message.get("role") == "tool" and "TARGET_WORKSPACE_671" in json.dumps(message) for request in observed for message in request["messages"]), "resumed session read from the configured default workspace"
            assert json.loads((root / "openclaw.json").read_text())["agents"]["defaults"]["workspace"] == str(default_workspace)
        run([bridge, "--snapshot", runtime, sid, str(root / "after.jsonl")], env, workspace)
        after = (root / "after.jsonl").read_bytes()
        assert after.startswith(before), "native continuation rewrote the archived baseline"
        assert b"RESTORE_CONFIRMED_671" in after
        if runtime != "openclaw":
            assert any("mcp__agit" in json.dumps(tool) or "mcp_agit" in json.dumps(tool) for request in observed for tool in request.get("tools", [])), "MCP tools were not discovered"
        assert "RESTORE_CONFIRMED_671" in agit(["show", target, "--json"]), "completion hook did not settle the new reply"
        for attempt in range(2):
            exported = root / f"continued-export-{attempt}.jsonl"
            agit(["export", target, "--format", runtime, "-o", str(exported)])
            for marker in ("ARCHIVE_MARKER_671", "TOOL_MARKER_671", "RESTORE_CONFIRMED_671"):
                assert marker in exported.read_text(), "export lost an inherited source"
            prepared = agit(["resume", target, "--as", runtime, "--no-launch", "--force"])
            next_sid = re.search(r"--(?:resume|session-id) '([^']+)'", prepared).group(1)
            command = [next_sid if arg == sid else arg for arg in command]
            sid = next_sid
            request_start = len(requests)
            run(command, env, workspace)
            continued = requests[request_start:]
            observed = [request for request in continued if request.get("stream")] if runtime == "hermes" else continued
            assert observed
            messages = json.dumps(observed[0]["messages"])
            for marker in ("ARCHIVE_MARKER_671", "TOOL_MARKER_671", "RESTORE_CONFIRMED_671"):
                assert marker in messages, "repeated restoration lost an inherited source"
            archived = agit(["show", target, "--json"])
            assert archived.count("RESTORE_CONFIRMED_671") >= attempt + 2, "repeated continuation was not saved"
        (root / "requests.json").write_text(json.dumps(requests, indent=2))
        print(json.dumps({"runtime": runtime, "restore": "pass", "resume": "pass", "automatic_capture": "pass", "repeated_restore": "pass", "public_new": "pass", "public_export": "pass", "public_resume_from": source_runtime, "mcp": "native plugin" if runtime == "openclaw" else "pass", "evidence": str(root)}), flush=True)
    except BaseException:
        print(json.dumps({"runtime": runtime, "status": "failed", "evidence": str(root)}), file=sys.stderr)
        raise
    finally:
        server.shutdown()
        server.server_close()


def main():
    repository = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runtime", choices=("all", "openclaw", "hermes", "workbuddy"))
    parser.add_argument("--agit", default=str(repository / "target/debug/agit"))
    parser.add_argument("--bridge", default=str(repository / "target/debug/examples/native-runtime-probe"))
    args = parser.parse_args()
    exe, bridge = str(Path(args.agit).resolve()), str(Path(args.bridge).resolve())
    for path in (exe, bridge):
        if not Path(path).is_file():
            parser.error("Run cargo build --bin agit --example native-runtime-probe first")
    for runtime in ("workbuddy", "hermes", "openclaw") if args.runtime == "all" else (args.runtime,):
        source_runtime = ("hermes" if runtime == "workbuddy" else "workbuddy") if args.runtime == "all" else None
        exercise(runtime, exe, bridge, repository, source_runtime)


if __name__ == "__main__":
    main()
