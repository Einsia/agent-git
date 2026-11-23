"""Prepare a local text-only capture from a new Antigravity CLI headless run."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid


MAX_BYTES = 4 * 1024 * 1024


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def read_prompts(raw):
    require(0 < len(raw) <= MAX_BYTES, "input is empty or too large")
    prompts = []
    for line in raw.splitlines():
        record = json.loads(line)
        require(isinstance(record, dict) and set(record) == {"event", "message"}
                and record["event"] == "user", "expected a headless user input")
        message = record["message"]
        require(isinstance(message, dict) and set(message) == {"content"}
                and isinstance(message["content"], str) and message["content"].strip(),
                "only nonempty text prompts can be captured")
        prompts.append(message["content"])
    return prompts


def collect(stream, prompts):
    identity = None
    turns = []
    total = 0
    user_step = None
    while line := stream.readline(MAX_BYTES - total + 1):
        total += len(line)
        require(total <= MAX_BYTES, "output exceeds capture limit")
        record = json.loads(line)
        kind = record["event"]
        if identity is None:
            require(kind == "init", "missing initial conversation identity")
            identity = record["conversation_id"]
            require(isinstance(identity, str) and str(uuid.UUID(identity)) == identity,
                    "invalid conversation identity")
            continue
        require(kind in {"step_update", "result"}, "unsupported stream event")
        body = record[kind]
        require(body["conversation_id"] == identity and len(turns) < len(prompts),
                "unexpected conversation or extra turn")
        if kind == "step_update":
            step = body["step_type"]
            require(step in {"user_input", "agent_response", "error_message"},
                    "this capture supports text turns only")
            if step == "user_input":
                index = body["step_index"]
                require(type(index) is int and index >= 0 and user_step in (None, index),
                        "ambiguous user input")
                user_step = index
            continue
        ordinal = len(turns) + 1
        require(user_step is not None and body["status"] == "SUCCESS"
                and type(body["num_turns"]) is int and body["num_turns"] == ordinal,
                "incomplete or out-of-order turn")
        reply = body["response"]
        require(isinstance(reply, str) and reply.strip(), "missing assistant reply")
        turns.append({"format": "antigravity-cli-text-turn-v1", "conversation_id": identity,
                      "turn": ordinal, "prompt": prompts[ordinal - 1], "reply": reply})
        user_step = None
    require(turns and len(turns) == len(prompts), "not every prompt received a reply")
    return turns


def publish_local(turns):
    root = (Path(os.environ.get("AGIT_HOME") or Path.home() / ".agit")
            / "captures" / "antigravity-cli")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    identity = turns[0]["conversation_id"]
    output = tempfile.NamedTemporaryFile(dir=root, delete=False)
    temporary = Path(output.name)
    try:
        with output:
            for turn in turns:
                output.write((json.dumps(turn, ensure_ascii=False) + "\n").encode())
        os.link(temporary, root / (identity + ".jsonl"))
    finally:
        temporary.unlink()
    return identity


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prompts", required=True, type=Path,
                        help="explicit NDJSON file of text prompts to send to agy")
    args = parser.parse_args()
    with args.prompts.open("rb") as source:
        raw = source.read(MAX_BYTES + 1)
    prompts = read_prompts(raw)
    with tempfile.TemporaryFile() as supplied:
        supplied.write(raw)
        supplied.seek(0)
        with subprocess.Popen(["agy", "--disable-slash-commands", "--input-format", "stream-json",
                               "--output-format", "stream-json", "--print-timeout", "2m"],
                              stdin=supplied, stdout=subprocess.PIPE) as process:
            try:
                turns = collect(process.stdout, prompts)
                require(process.wait() == 0, "agy did not complete successfully")
            finally:
                if process.poll() is None:
                    process.kill()
    print(publish_local(turns))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, TypeError, UnicodeError):
        sys.exit("No capture published: expected a complete text-only stream and a new local capture file.")
