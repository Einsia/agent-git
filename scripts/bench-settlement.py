#!/usr/bin/env python3
"""Measure complete native settlement on synthetic transcripts without contacting a Hub."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def measure(binary, turns, events, payload_bytes, git_bin_dir):
    with tempfile.TemporaryDirectory(prefix="agit-settlement-bench-") as temporary:
        root = Path(temporary)
        home, store, work = (root / name for name in ("home", "store", "work"))
        work.mkdir()
        credentials = store / "credentials"
        credentials.mkdir(parents=True)
        hub = "https://benchmark.invalid"
        (credentials / "benchmark.invalid.json").write_text(json.dumps({
            "username": "bench", "email": "bench@example.invalid", "hub": hub,
            "access_token": "synthetic", "refresh_token": "synthetic",
            "access_expires_at": "2099-01-01T00:00:00Z",
            "refresh_expires_at": "2099-01-01T00:00:00Z",
        }))
        session = "aaaaaaaa-0000-4000-8000-000000000001"
        project = home / ".claude" / "projects" / re.sub(r"[^a-zA-Z0-9]", "-", str(work))
        project.mkdir(parents=True)
        transcript = project / f"{session}.jsonl"
        with transcript.open("w") as output:
            for turn in range(turns):
                output.write(json.dumps({"type": "user", "sessionId": session, "cwd": str(work),
                    "uuid": f"user-{turn}", "message": {"role": "user", "content": f"Synthetic turn {turn}"}}) + "\n")
                for event in range(events):
                    output.write(json.dumps({"type": "assistant", "sessionId": session,
                        "uuid": f"assistant-{turn}-{event}", "message": {"role": "assistant",
                        "content": f"Synthetic event {turn}/{event}: " + "narrative " * (payload_bytes // 10)}}) + "\n")
        path = os.environ.get("PATH", "")
        if git_bin_dir:
            path = str(git_bin_dir) + os.pathsep + path
        env = {"PATH": path, "HOME": str(home), "AGIT_HOME": str(store), "AGIT_HUB_URL": hub,
               "AGIT_SECRETS_KEYSTORE": "file", "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_CONFIG_GLOBAL": str(home / "empty-gitconfig"), "GIT_TERMINAL_PROMPT": "0",
               "CI": "1", "NO_COLOR": "1"}
        for name in ("SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"):
            if name in os.environ:
                env[name] = os.environ[name]
        def run(*args):
            result = subprocess.run([str(binary), *args, "--no-tui"], cwd=work, env=env,
                                    capture_output=True, text=True, timeout=900)
            if result.returncode:
                raise RuntimeError(result.stdout + result.stderr)
            return result
        run("init", "bench")
        started = time.perf_counter()
        run("import", session, "--from", "claude-code", "--into", "bench/bench@session")
        elapsed = time.perf_counter() - started
        repo = store / "repos" / "bench" / "bench"
        count = int(subprocess.check_output(["git", "-C", str(repo), "rev-list", "--count", "main..session"], text=True)) - 1
        assert count == turns, (count, turns)
        return {"turns": turns, "events_per_turn": events, "transcript_bytes": transcript.stat().st_size,
                "seconds": round(elapsed, 3), "committed_turns": count}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--turns", default="10,20,40")
    parser.add_argument("--events-per-turn", type=int, default=50)
    parser.add_argument("--payload-bytes", type=int, default=1024)
    parser.add_argument("--git-bin-dir", type=Path, help="Optional baseline-only Git wrapper directory")
    args = parser.parse_args()
    for turns in map(int, args.turns.split(",")):
        if turns <= 0 or args.events_per_turn <= 0 or args.payload_bytes < 0:
            parser.error("turns and events must be positive; payload bytes must be nonnegative")
        print(json.dumps(measure(args.binary.resolve(), turns, args.events_per_turn,
                                 args.payload_bytes, args.git_bin_dir.resolve() if args.git_bin_dir else None)), flush=True)


if __name__ == "__main__":
    main()
