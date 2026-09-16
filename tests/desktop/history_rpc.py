"""Exercise native history, watch overlap and immutable paging through a real daemon."""
import asyncio
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import uuid

from launch_rpc import Client, eventually


class WatchingClient(Client):
    async def connect(self, endpoint):
        self.events = []
        return await super().connect(endpoint)

    async def read(self):
        try:
            while line := await self.reader.readline():
                frame = json.loads(line)
                future = self.pending.pop(frame.get("id"), None)
                if future is not None and not future.done():
                    future.set_result(frame)
                elif frame.get("method"):
                    self.events.append(frame)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError("owner RPC closed"))


def fixtures(root):
    project = root / "project"
    project.mkdir()
    codex, claude = str(uuid.uuid4()), str(uuid.uuid4())
    codex_file = root / "codex" / "sessions" / f"rollout-2026-09-16T00-00-00-{codex}.jsonl"
    claude_file = root / "claude" / "projects" / str(project).replace("/", "-") / f"{claude}.jsonl"
    for file in (codex_file, claude_file):
        file.parent.mkdir(parents=True)
    stamp = "2026-09-16T00:00:00Z"
    rows = [dict(type="session_meta", payload=dict(id=codex, cwd=str(project), source="cli", timestamp=stamp, history_mode="model"))]
    rows += [dict(type="response_item", payload=dict(type="message", role="assistant", content=[dict(type="output_text", text="Repeated answer")])) for _ in range(150)]
    codex_file.write_text("".join(json.dumps(row) + "\n" for row in rows))
    claude_file.write_text("".join(json.dumps(dict(type="assistant", uuid=str(uuid.uuid4()), sessionId=claude, cwd=str(project), timestamp=stamp,
        message=dict(role="assistant", content=[dict(type="text", text="Repeated answer")]))) + "\n" for _ in range(150)))
    database = root / "xdg" / "opencode" / "opencode.db"
    database.parent.mkdir(parents=True)
    with sqlite3.connect(database) as db:
        db.executescript("""
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
            CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,
                directory TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, version TEXT NOT NULL);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
        """)
        db.execute("INSERT INTO project VALUES (?, ?)", ("project", str(project)))
        db.execute("INSERT INTO session VALUES (?, ?, NULL, ?, 1, 1, '1.18.13')", ("ses_history", "project", str(project)))
        for index in range(150):
            db.execute("INSERT INTO message VALUES (?, ?, ?, ?)", (f"msg_{index:04}", "ses_history", index * 2, json.dumps(dict(role="assistant", finish="stop"))))
            db.execute("INSERT INTO part VALUES (?, ?, ?, ?, ?)", (f"prt_{index:04}", f"msg_{index:04}", "ses_history", index * 2 + 1, json.dumps(dict(type="text", text="Repeated answer"))))
    return project, [("codex", codex, codex_file), ("claude-code", claude, claude_file), ("opencode", "ses_history", database)]


async def run(binary, evidence=None, serve=False):
    with tempfile.TemporaryDirectory(prefix="agit-history-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        project, sources = fixtures(root)
        binaries = root / "bin"
        binaries.mkdir()
        for name in ("codex", "claude", "opencode"):
            executable = binaries / name
            executable.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "fixture 1.0.0"; exit 0; fi\nexit 77\n')
            executable.chmod(0o700)
        env = dict(os.environ, PATH=str(binaries) + os.pathsep + os.environ["PATH"], AGIT_HOME=str(root / "agit"), CODEX_HOME=str(root / "codex"),
                   CLAUDE_CONFIG_DIR=str(root / "claude"), XDG_DATA_HOME=str(root / "xdg"))
        endpoint = root / "agit" / "desktop-rc" / "control.rpc"
        client = None
        result = []
        with (root / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([binary, "rc", "local", "start"], env=env, stdout=log, stderr=log)
            try:
                async def ready():
                    assert daemon.poll() is None, (root / "daemon.log").read_text()
                    return endpoint.exists()
                await eventually(ready, "owner RPC did not open")
                client = await WatchingClient().connect(str(endpoint))
                description = (await client.rpc("machine.describe"))["result"]
                assert description["history"] == dict(version=2, snapshot=True, runtimes=["codex", "claude-code", "opencode"])
                bound = await client.rpc("project.bind", project_id="history", local_path=str(project))
                assert "result" in bound, bound
                if serve:
                    Path(evidence).write_text(json.dumps(dict(endpoint=str(endpoint), sources=[dict(runtime=runtime, session_id=native, cwd=str(project)) for runtime, native, _ in sources])))
                    print("History fixture daemon ready", flush=True)
                    await asyncio.Event().wait()
                for runtime, native, path in sources:
                    params = dict(session_id=native, runtime=runtime, cwd=str(project))
                    watch = await client.rpc("session.watch", session_id=native)
                    assert "result" in watch, watch
                    stream = watch["result"]["session"]["session_id"]
                    async def complete():
                        return any(frame.get("stream") == stream and frame.get("method") == "session.history.status" and frame["params"]["status"] == "complete" for frame in client.events)
                    await eventually(complete, f"{runtime} watch did not finish initial history")
                    watched = [frame for frame in client.events if frame.get("stream") == stream and frame.get("method") == "item.completed"]
                    reply = await client.rpc("session.history", **params)
                    assert "result" in reply, reply
                    first = reply["result"]
                    assert first["has_more"] and first["status"] == "complete", first
                    if runtime == "opencode":
                        with sqlite3.connect(path) as db:
                            db.execute("INSERT INTO message VALUES ('msg_live', 'ses_history', 999, ?)", (json.dumps(dict(role="assistant", finish="stop")),))
                            db.execute("INSERT INTO part VALUES ('prt_live', 'msg_live', 'ses_history', 1000, ?)", (json.dumps(dict(type="text", text="Live appended answer")),))
                    else:
                        row = json.loads(path.read_text().splitlines()[-1])
                        if runtime == "codex":
                            row["payload"]["content"][0]["text"] = "Live appended answer"
                        else:
                            row["uuid"] = str(uuid.uuid4())
                            row["message"]["content"][0]["text"] = "Live appended answer"
                        with path.open("a") as output:
                            output.write(json.dumps(row) + "\n")
                    async def appended():
                        return any(frame.get("stream") == stream and frame.get("method") == "item.completed" and frame["params"]["event"].get("text") == "Live appended answer" for frame in client.events)
                    await eventually(appended, f"{runtime} live append was not delivered")
                    # Native writers may replace earlier data while a reader owns a snapshot.
                    if runtime == "opencode":
                        with sqlite3.connect(path) as db:
                            db.execute("UPDATE part SET data = ? WHERE id = 'prt_0000'", (json.dumps(dict(type="text", text="Revised answer")),))
                    else:
                        replacement = path.with_suffix(".replacement")
                        replacement.write_text(path.read_text().replace("Repeated answer", "Revised answers"))
                        replacement.replace(path)
                    pages = [first]
                    while pages[-1]["has_more"]:
                        reply = await client.rpc("session.history", **params, before=pages[-1]["before"], snapshot=first["snapshot"])
                        assert "result" in reply, reply
                        page = reply["result"]
                        assert page["snapshot"] == first["snapshot"] and page["before"] < pages[-1]["before"]
                        pages.append(page)
                    items = [item for page in reversed(pages) for item in page["items"]]
                    assert len(items) == 150, (runtime, len(items))
                    assert all(item["event"]["text"] == "Repeated answer" for item in items)
                    ids = {item["source_id"] for item in items}
                    assert len(ids) == 150 and ids == {frame["params"]["source_id"] for frame in watched}, runtime
                    async def revised():
                        return any(frame.get("stream") == stream and frame.get("method") == "item.completed" and frame["params"]["event"].get("text", "").startswith("Revised answer") for frame in client.events)
                    await eventually(revised, f"{runtime} source revision was not delivered")
                    if runtime != "opencode":
                        assert any(frame.get("stream") == stream and frame.get("method") == "session.history.status" and frame["params"]["status"] == "reset" for frame in client.events)
                    fresh = (await client.rpc("session.history", **params))["result"]
                    assert fresh["snapshot"] != first["snapshot"]
                    expired = await client.rpc("session.history", **params, before=first["before"], snapshot="expired")
                    assert expired["error"]["data"]["restart"] is True
                    assert expired["error"]["data"]["kind"] == "cursor_expired"
                    wrong_scope = await client.rpc("session.history", **(params | dict(session_id="another")), snapshot=first["snapshot"])
                    assert wrong_scope["error"]["data"]["kind"] == "cursor_expired"
                    legacy_cursor = await client.rpc("session.history", **params, before=first["before"])
                    assert legacy_cursor["error"]["data"]["kind"] == "cursor_expired"
                    result.append(dict(runtime=runtime, pages=pages, watch=watched))
                    print(f"{runtime}: native paging, watch identity, repeated records and immutable snapshot passed", flush=True)
                # An empty source and malformed source have different outcomes.
                runtime, native, path = sources[1]
                params = dict(session_id=native, runtime=runtime, cwd=str(project))
                path.write_text("")
                empty = (await client.rpc("session.history", **params))["result"]
                assert empty["items"] == [] and empty["before"] == 0 and empty["status"] == "complete"
                with path.open("wb") as output:
                    output.truncate(300 * 1024 * 1024)
                limited = await client.rpc("session.history", **params)
                assert limited["error"]["data"]["kind"] == "resource_limit", limited
                path.write_text("not-json\n")
                broken = await client.rpc("session.history", **params)
                assert broken["error"]["data"]["kind"] == "invalid_record", broken
                path.write_text('{"partial":')
                pending = await client.rpc("session.history", **params)
                assert pending["error"]["data"]["kind"] == "incomplete_record", pending
                path.unlink()
                missing = await client.rpc("session.history", **params)
                assert missing["error"]["data"]["kind"] == "source_missing", missing
                print("empty, corrupt, partial and missing history report distinct results", flush=True)
                if evidence:
                    Path(evidence).write_text(json.dumps(result))
            finally:
                if client:
                    await client.close()
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    serve = len(sys.argv) > 2 and sys.argv[2] == "--serve"
    asyncio.run(run(str(Path(sys.argv[1]).resolve()), sys.argv[3] if serve else sys.argv[2] if len(sys.argv) > 2 else None, serve))
