#!/usr/bin/env python3
"""Copy every upstream archive that git-runtime-lock.json pins into the project's package mirror.

prepare-git-runtime.py reads the mirror before upstream, so a build keeps working after upstream
removes a pinned version, but only for archives the mirror already holds. This uploads the ones it
lacks. A file is uploaded only after its bytes match the pin, and a mirror copy that does not match
its pin fails the run instead of being replaced, so the mirror never holds an unverified archive.
"""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).with_name("prepare-git-runtime.py"))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


def upstream_pins(lock):
    """Yield each pin a build downloads, once per digest and file name.

    A pin with a vendored path never reaches the network, and neither do the provenance archives
    recorded beside it, so the mirror has nothing to hold for either.
    """
    seen = set()
    pending = [lock]
    while pending:
        node = pending.pop()
        if isinstance(node, dict):
            if "path" in node:
                continue
            if "url" in node and "sha256" in node:
                key = (node["sha256"], node["url"].rsplit("/", 1)[-1])
                if key not in seen:
                    seen.add(key)
                    yield node
            pending.extend(node.values())
        elif isinstance(node, list):
            pending.extend(node)


def send(request, timeout):
    """Return the response body, retrying server-side and connection failures a bounded number of times."""
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.read()
        except OSError as error:
            transient = not isinstance(error, urllib.error.HTTPError) or error.code >= 500 or error.code == 429
            if not transient or attempt == 2:
                raise
            time.sleep(2 ** attempt)


def mirrored_digest(url, token):
    """Return the SHA-256 of the mirror's copy, or None when the mirror has no copy."""
    try:
        return hashlib.sha256(send(urllib.request.Request(url, headers={"JOB-TOKEN": token}), 120)).hexdigest()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def mirror_missing(lock, api, project, token, cache):
    """Upload each verified pin the mirror lacks and return a description of every pin it could not."""
    failures = []
    for pin in upstream_pins(lock):
        name = pin["url"].rsplit("/", 1)[-1]
        url = runtime.mirror_url(pin, api, project)
        try:
            digest = mirrored_digest(url, token)
            if digest == pin["sha256"]:
                print(f"mirrored {name}", flush=True)
                continue
            if digest is not None:
                failures.append(f"the mirror's copy of {name} does not match its pin {pin['sha256']}")
                continue
            body = runtime.download(pin, cache)
            if hashlib.sha256(body).hexdigest() != pin["sha256"]:
                failures.append(f"{pin['url']} does not match its pin and was not uploaded")
                continue
            send(urllib.request.Request(url, data=body, method="PUT", headers={"JOB-TOKEN": token}), 600)
            if mirrored_digest(url, token) != pin["sha256"]:
                failures.append(f"the uploaded copy of {name} does not read back with its pin")
                continue
        except (OSError, RuntimeError, ValueError) as error:
            failures.append(f"{name} was not mirrored: {error}")
            continue
        print(f"uploaded {name}", flush=True)
    return failures


def main():
    lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
    with tempfile.TemporaryDirectory() as cache:
        failures = mirror_missing(
            lock, os.environ["CI_API_V4_URL"], os.environ["CI_PROJECT_ID"], os.environ["CI_JOB_TOKEN"], Path(cache)
        )
    for failure in failures:
        print("FAILED: " + failure, file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
