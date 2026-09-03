#!/usr/bin/env python3
"""Run agit's Codex hooks against one real, isolated Codex session.

The probe copies only the Codex authentication file into a temporary CODEX_HOME,
installs hooks there through `agit setup`, and removes the directory on exit. It
never reads or writes the user's hooks.json or config.toml.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile


EVENTS = ("SessionStart", "Stop")
REPO = "local/codex-hook-probe"
BRANCH = "probe"
HUB = "http://127.0.0.1:1"


def record_event(label: str, output: Path) -> int:
    raw = sys.stdin.read()
    try:
        payload: object = json.loads(raw)
    except json.JSONDecodeError:
        payload = {"unparsed": raw}
    with output.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps({"event": label, "payload": payload}) + "\n")
    return 0


def run(command: list[str], *, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=180)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
        stderr = f"{stderr}\nprobe killed the process group after the timeout"
        return subprocess.CompletedProcess(command, 124, stdout, stderr)
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def require_success(result: subprocess.CompletedProcess[str], label: str) -> None:
    if result.returncode == 0:
        return
    detail = (result.stderr or result.stdout).strip()
    raise RuntimeError(f"{label} failed with exit code {result.returncode}:\n{detail}")


def add_recorders(hooks_path: Path, capture_path: Path) -> None:
    document = json.loads(hooks_path.read_text(encoding="utf-8"))
    hooks = document["hooks"]
    script = Path(__file__).resolve()
    for event in EVENTS:
        command = " ".join(
            shlex.quote(part)
            for part in (
                sys.executable,
                str(script),
                "--record",
                event,
                "--output",
                str(capture_path),
            )
        )
        hooks.setdefault(event, []).append(
            {"hooks": [{"type": "command", "command": command, "timeout": 5}]}
        )
    hooks_path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def installed_commands(hooks_path: Path) -> dict[str, list[str]]:
    document = json.loads(hooks_path.read_text(encoding="utf-8"))
    return {
        event: [
            hook["command"]
            for group in document.get("hooks", {}).get(event, [])
            for hook in group.get("hooks", [])
            if isinstance(hook, dict) and isinstance(hook.get("command"), str)
        ]
        for event in EVENTS
    }


def read_capture(path: Path) -> list[dict[str, object]]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def validate_capture(events: list[dict[str, object]], agit_home: Path) -> None:
    by_event = {str(item.get("event")): item.get("payload") for item in events}
    missing = [event for event in EVENTS if event not in by_event]
    if missing:
        raise RuntimeError(f"Codex did not invoke these configured hooks: {', '.join(missing)}")

    starts = by_event["SessionStart"]
    stops = by_event["Stop"]
    if not isinstance(starts, dict) or not isinstance(stops, dict):
        raise RuntimeError("Codex hook stdin was not a JSON object")

    required = ("session_id", "cwd", "hook_event_name", "transcript_path")
    for label, payload in (("SessionStart", starts), ("Stop", stops)):
        absent = [key for key in required if not payload.get(key)]
        if absent:
            raise RuntimeError(f"{label} omitted required fields: {', '.join(absent)}")
    if starts["session_id"] != stops["session_id"]:
        raise RuntimeError("SessionStart and Stop described different sessions")
    if starts["transcript_path"] != stops["transcript_path"]:
        raise RuntimeError("SessionStart and Stop described different transcripts")
    if starts.get("hook_event_name") != "SessionStart":
        raise RuntimeError("SessionStart used the wrong hook_event_name")
    if stops.get("hook_event_name") != "Stop":
        raise RuntimeError("Stop used the wrong hook_event_name")
    if starts.get("source") != "startup":
        raise RuntimeError(f"unexpected SessionStart source: {starts.get('source')!r}")

    link = agit_home / "store" / "codex" / f"{starts['session_id']}.json"
    if not link.is_file():
        raise RuntimeError("agit's installed SessionStart hook did not register the Codex session")
    binding = json.loads(link.read_text(encoding="utf-8"))
    if binding.get("owner") != "local" or binding.get("agent") != "codex-hook-probe":
        raise RuntimeError("agit's installed SessionStart hook registered the wrong repository")
    if binding.get("branch") != BRANCH:
        raise RuntimeError("agit's installed SessionStart hook registered the wrong branch")


def revision(repo_path: Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo_path), "rev-parse", f"refs/heads/{BRANCH}"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.stdout.strip()


def seed_local_author(agit_home: Path) -> None:
    credentials = agit_home / "credentials" / "127.0.0.1_1.json"
    credentials.parent.mkdir(parents=True, exist_ok=True)
    credentials.write_text(
        json.dumps(
            {
                "username": "probe",
                "email": "probe@example.invalid",
                "hub": HUB,
                "access_token": "probe",
                "access_expires_at": "2099-01-01T00:00:00Z",
                "refresh_token": "probe",
                "refresh_expires_at": "2099-01-01T00:00:00Z",
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    credentials.chmod(0o600)


def probe(args: argparse.Namespace) -> int:
    agit = Path(args.agit).resolve()
    codex = Path(args.codex).resolve()
    configured_home = os.environ.get("CODEX_HOME", "").strip()
    source_home = Path(configured_home) if configured_home else Path.home() / ".codex"
    source_auth = source_home / "auth.json"
    if not source_auth.is_file():
        raise RuntimeError(f"Codex is not logged in through {source_auth}")

    with tempfile.TemporaryDirectory(prefix="agit-codex-hook-probe.") as temporary:
        root = Path(temporary)
        home = root / "home"
        codex_home = root / "codex"
        agit_home = root / "agit"
        work = root / "work"
        for directory in (home, codex_home, agit_home, work):
            directory.mkdir()
        copied_auth = codex_home / "auth.json"
        shutil.copy2(source_auth, copied_auth)
        copied_auth.chmod(0o600)

        env = dict(os.environ)
        env.update(
            HOME=str(home),
            CODEX_HOME=str(codex_home),
            AGIT_HOME=str(agit_home),
            AGIT_HUB_URL=HUB,
            AGIT_TUI="0",
        )
        for name in (
            "AGIT_SESSION",
            "CODEX_SESSION_ID",
            "CODEX_THREAD_ID",
            "CLAUDE_SESSION_ID",
            "CLAUDE_CODE_SESSION_ID",
        ):
            env.pop(name, None)

        require_success(run(["git", "init", "-q"], cwd=work, env=env), "git init")
        require_success(
            run([str(agit), "--no-tui", "init", "codex-hook-probe"], cwd=work, env=env),
            "agit init",
        )
        require_success(
            run(
                [
                    str(agit),
                    "--no-tui",
                    "--yes",
                    "new",
                    REPO,
                    "--branch",
                    BRANCH,
                    "--as",
                    "codex",
                    "--no-launch",
                ],
                cwd=work,
                env=env,
            ),
            "agit new",
        )
        repo_result = run([str(agit), "repo", "path", REPO], cwd=work, env=env)
        require_success(repo_result, "agit repo path")
        repo_path = Path(repo_result.stdout.strip())
        before = revision(repo_path)
        # Hook settlement needs a commit author. A local fake identity exercises that path without
        # copying AgentGit credentials or contacting a hub.
        seed_local_author(agit_home)
        setup = run(
            [str(agit), "--no-tui", "setup", "--runtime", "codex", "--hooks"],
            cwd=work,
            env=env,
        )
        require_success(setup, "agit setup")

        hooks_path = codex_home / "hooks.json"
        commands = installed_commands(hooks_path)
        if not any("hooks ingest --runtime codex" in command for command in commands["SessionStart"]):
            raise RuntimeError("agit setup did not install the Codex SessionStart command")
        if not any("hooks settle --runtime codex" in command for command in commands["Stop"]):
            raise RuntimeError("agit setup did not install the Codex Stop command")

        capture = root / "events.jsonl"
        add_recorders(hooks_path, capture)
        env["AGIT_SESSION"] = f"{REPO}@{BRANCH}"
        result = run(
            [
                str(codex),
                "--dangerously-bypass-hook-trust",
                "--sandbox",
                "read-only",
                "--ask-for-approval",
                "never",
                "--disable",
                "apps",
                "--disable",
                "plugins",
                "exec",
                "--skip-git-repo-check",
                "--color",
                "never",
                "Reply with exactly CODEX_HOOK_PROBE_OK. Do not use tools.",
            ],
            cwd=work,
            env=env,
        )
        require_success(result, "codex exec")

        events = read_capture(capture)
        validate_capture(events, agit_home)
        after = revision(repo_path)
        if before == after:
            raise RuntimeError("agit's installed Stop hook did not settle the Codex turn")
        print(json.dumps(events, indent=2, sort_keys=True))
        print(f"branch {BRANCH} advanced from {before[:12]} to {after[:12]}")
        print("Codex SessionStart ingestion and Stop settlement passed end-to-end.")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--agit",
        default=str(Path(__file__).resolve().parents[1] / "target" / "debug" / "agit"),
    )
    parser.add_argument("--codex", default=shutil.which("codex") or "codex")
    parser.add_argument("--record", metavar="EVENT", help=argparse.SUPPRESS)
    parser.add_argument("--output", type=Path, help=argparse.SUPPRESS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.record:
        if args.output is None:
            raise RuntimeError("--record needs --output")
        return record_event(args.record, args.output)
    return probe(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
