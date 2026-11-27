"""Exercise first import and lossless readback using an isolated CLI test account."""

import argparse
import hashlib
import itertools
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import uuid


def generate(path, session, work, target_bytes, tail="none"):
    timestamp = "2026-01-01T00:00:00Z"
    padding = "ordinary conversation text " * 2000
    with path.open("xb") as stream:
        def record(kind, **payload):
            value = {"timestamp": timestamp, "type": kind, "payload": payload}
            stream.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")

        record("session_meta", id=session, cwd=str(work), timestamp=timestamp, source="cli")
        for turn in itertools.count(1):
            turn_id = f"turn-{turn}"
            record("event_msg", type="task_started", turn_id=turn_id)
            record("response_item", type="message", role="user", content=[
                {"type": "input_text", "text": f"Question {turn}"}])
            for index in range(128):
                record("response_item", type="message", role="assistant", content=[
                    {"type": "output_text", "text": f"Answer {turn}.{index}: {padding}"}])
            record("event_msg", type="task_complete", turn_id=turn_id)
            if turn == 1:
                record("event_msg", type="task_started", turn_id="wake")
                record("response_item", type="message", role="assistant", content=[
                    {"type": "output_text", "text": "Continuation without a prompt"}])
                record("event_msg", type="task_complete", turn_id="wake")
            if stream.tell() >= target_bytes:
                settled = stream.tell()
                if tail == "bookkeeping":
                    record("event_msg", type="token_count")
                elif tail == "incomplete":
                    record("event_msg", type="task_started", turn_id="pending")
                    record("response_item", type="message", role="user", content=[
                        {"type": "input_text", "text": "Unfinished question"}])
                return turn, settled


def compare(original, exported, settled):
    digest = hashlib.sha256()
    with original.open("rb") as source, exported.open("rb") as saved:
        count = 0
        for count, after in enumerate(saved, 1):
            before = source.readline()
            if not before or json.loads(before) != json.loads(after):
                raise ValueError(f"Export differs at record {count}")
            digest.update(before)
        if source.tell() != settled:
            raise ValueError("Export does not end at the last completed execution")
    return count, digest.hexdigest()


def measure_import(command, work, env):
    peak = 0
    with subprocess.Popen(command, cwd=work, env=env) as process:
        while process.poll() is None:
            rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,rss="], text=True)
            children = {}
            sizes = {}
            for row in rows.splitlines():
                pid, parent, rss = map(int, row.split())
                children.setdefault(parent, []).append(pid)
                sizes[pid] = rss * 1024
            pending = [process.pid]
            total = 0
            while pending:
                pid = pending.pop()
                total += sizes.get(pid, 0)
                pending.extend(children.get(pid, ()))
            peak = max(peak, total)
            try:
                process.wait(timeout=0.5)
            except subprocess.TimeoutExpired:
                pass
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, command)
    print(f"Sampled peak import process-tree RSS: {peak} bytes", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agit", default="agit")
    parser.add_argument("--mib", default=600, type=int)
    parser.add_argument("--tail", choices=("none", "bookkeeping", "incomplete"), default="none")
    parser.add_argument("--into", required=True, help="A fresh owner/repo@branch in the test account")
    args = parser.parse_args()
    if not os.environ.get("AGIT_HOME") or not os.environ.get("AGIT_HUB_URL"):
        parser.error("Set AGIT_HOME and AGIT_HUB_URL for an isolated test account")
    if args.mib <= 0:
        parser.error("--mib must be positive")
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory).resolve()
        runtime = work / "codex"
        session = str(uuid.uuid4())
        source = runtime / "sessions/2026/01/01" / f"rollout-2026-01-01T00-00-00-{session}.jsonl"
        source.parent.mkdir(parents=True)
        turns, settled = generate(source, session, work, args.mib * 1024**2, args.tail)
        print(f"Source: {source.stat().st_size} bytes, session {session}", flush=True)
        env = {**os.environ, "CODEX_HOME": str(runtime), "CODEX_THREAD_ID": session}
        exported = work / "export.jsonl"
        measure_import([args.agit, "import", session, "--from", "codex", "--independent",
                        "--into", args.into, "--yes"], work, env)
        usage = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        print(f"Import peak RSS: {usage * (1 if sys.platform == 'darwin' else 1024)} bytes", flush=True)
        slug, branch = args.into.rsplit("@", 1)
        checkout = Path(env["AGIT_HOME"]) / "repos" / slug
        def git(*arguments):
            return subprocess.check_output(["git", "-C", checkout, *arguments], text=True).strip()
        claim = None
        ordinals = []
        for commit in git("rev-list", "--reverse", f"main..{branch}").splitlines():
            meta = json.loads(git("show", f"{commit}:session/meta.json"))
            if claim is None:
                claim = meta["session"]
            if meta["session"] != claim or (meta.get("turn") and session not in meta["runtime_instances"]):
                raise ValueError(f"Session identity changed at {commit}")
            if meta.get("turn"):
                ordinals.append(meta["turn"])
        if not claim or ordinals != list(range(1, turns + 1)):
            raise ValueError("Expected one session with one commit per user turn")
        subprocess.run([args.agit, "export", args.into, "-o", exported], check=True, cwd=work, env=env)
        count, digest = compare(source, exported, settled)
        link = Path(env["AGIT_HOME"]) / "store/codex" / f"{session}.json"
        checkpoint = json.loads(link.read_bytes())["native_checkpoint"]
        if (checkpoint["tip"], checkpoint["bytes"], checkpoint["sha256"]) != (git("rev-parse", branch), settled, digest):
            raise ValueError("Checkpoint does not identify the exported prefix")
        print(f"Verified {turns} turns, {count} ordered records, {settled} settled bytes, claim {claim}")


if __name__ == "__main__":
    main()
