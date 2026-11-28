"""Reconcile the native Windows queue once, from the operator host only."""

import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


QUEUE = "agentgit-cli-windows"
IDLE = 600
PROJECT = "projects/dev%2Fagentgit%2Fagent-git"


def command(args, payload=None):
    result = subprocess.run(args, input=payload, capture_output=True, text=True, timeout=40)
    if result.returncode:
        raise RuntimeError(f"{args[0]} {args[1]} failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def gitlab(endpoint, **update):
    args = ["glab", "api", "--hostname", os.environ["GITLAB_HOST"], endpoint]
    if update:
        return command(args + ["-X", "PUT", "--input", "-"], json.dumps(update))
    return command(args)


def pending_jobs():
    page = 1
    while True:
        jobs = gitlab(f"{PROJECT}/jobs?scope[]=pending&scope[]=running&per_page=100&page={page}")
        yield from (job for job in jobs if job["name"] == "windows:route" or QUEUE in job["tag_list"])
        if len(jobs) < 100:
            return
        page += 1


def ec2(action):
    return command(["aws", "ec2", action, "--instance-ids", os.environ["WINDOWS_INSTANCE_ID"],
                    "--output", "json", "--no-cli-pager"])


def log(message):
    print(datetime.now(timezone.utc).isoformat(), message, flush=True)


def admit(job):
    endpoint = f"{PROJECT}/packages/generic/windows-route/{job}/ready"
    subprocess.run(["glab", "api", "--hostname", os.environ["GITLAB_HOST"], "-X", "PUT", endpoint,
                    "--input", "-"], input="ready", text=True, check=True, timeout=40)
    log(f"AWS ready for routing job {job}")


def reconcile(state, save):
    now = int(time.time())
    jobs = list(pending_jobs())
    pending = {str(job["id"]) for job in jobs}
    if pending != set(state.get("pending", [])):
        log(f"pending jobs: {sorted(pending)}")
    state["pending"] = sorted(pending)
    state["admitted"] = sorted(pending.intersection(state.get("admitted", [])))
    save()
    runner = gitlab("runners/24")
    busy = runner.get("job_execution_status") == "active" or bool(gitlab("runners/24/jobs?status=running&per_page=1"))

    instance, = ec2("describe-instances")["Reservations"][0]["Instances"]
    if not any(tag == {"Key": "Role", "Value": QUEUE} for tag in instance.get("Tags", [])):
        raise ValueError("Instance is not the designated CLI Windows runner")
    power = instance["State"]["Name"]
    if power != state.get("power"):
        log(f"instance {power}")
        state["power"] = power
    if pending or busy or power != "running":
        state["idle_since"] = now
    if pending and power == "stopped" and (not runner["paused"] or state.get("draining")):
        ec2("start-instances")
        state["boot_since"] = now
        log(f"start requested for jobs {sorted(pending)}")
    if pending or busy:
        if state.get("draining") and power == "running":
            gitlab("runners/24", paused=False)
            state.pop("draining")
            log("AWS accepting jobs")
        if power == "running" and not runner["paused"] and runner["status"] == "online" and runner.get("contacted_at"):
            contact = datetime.fromisoformat(runner["contacted_at"])
            if contact.timestamp() >= max(now - 90, state.get("boot_since", 0)):
                for job in jobs:
                    key = str(job["id"])
                    if job["name"] == "windows:route" and key not in state["admitted"]:
                        admit(key)
                        state["admitted"].append(key)
                        save()
    elif power == "running" and now - state.setdefault("idle_since", now) >= IDLE:
        if not runner["paused"]:
            state["draining"] = True
            save()
            gitlab("runners/24", paused=True)
            log("AWS paused for idle drain")
        elif state.get("draining") and not gitlab("runners/24/jobs?status=running&per_page=1"):
            ec2("stop-instances")
            log("idle stop requested after drain")
    save()


def main():
    path = Path(sys.argv[1])
    state = json.loads(path.read_text()) if path.exists() else {}

    def save():
        temp = path.with_suffix(".tmp")
        temp.write_text(json.dumps(state))
        temp.replace(path)

    reconcile(state, save)


if __name__ == "__main__":
    main()
