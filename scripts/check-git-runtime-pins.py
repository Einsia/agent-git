#!/usr/bin/env python3
"""Report pinned runtime inputs that upstream or the package mirror no longer serves, and Alpine
pins that upstream replaced.

Every release build downloads the archives git-runtime-lock.json pins. Upstream can remove one at
any time (Alpine replaces a package's revision and drops the old file), and the GitHub release
build then fails on that download. This reads every pinned upstream URL, compares its bytes with
the pin, and for the Alpine pins also reads the branch's package index to name the version
upstream now carries. Inside a CI job it also reads the project's package mirror, which builds read
before upstream: a pin the mirror lost or holds with other bytes sends every build back upstream,
and nothing would say so. It changes nothing: a moved Alpine version is reported for a lock update
by hand, never applied, and a lost mirror copy is restored by the git-runtime:mirror job.

Exit status 1 when any pinned URL cannot be read or no longer matches its pin, or the package
mirror lacks a pin or holds other bytes for it: the next release build would fail, or every build
reads that pin from upstream again. Exit status 2 when every pin still downloads but upstream now
carries a newer version of an Alpine pin: the pinned file can disappear at any time, so the lock
should move before it does. Exit status 0 when neither holds.
"""

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import sys
import tarfile
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location("mirror", Path(__file__).with_name("mirror-git-runtime-archives.py"))
mirror = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mirror)

ALPINE_PACKAGE = re.compile(r"^(https://dl-cdn\.alpinelinux\.org/alpine/[^/]+/[^/]+/[^/]+)/[^/]+\.apk$")


def fetch(url):
    return mirror.send(urllib.request.Request(url), 120)


def alpine_index(directory):
    """Return {package name: version} from the APKINDEX of one Alpine repository directory."""
    with tarfile.open(fileobj=io.BytesIO(fetch(directory + "/APKINDEX.tar.gz")), mode="r:gz") as archive:
        text = archive.extractfile("APKINDEX").read().decode()
    versions = {}
    for record in text.split("\n\n"):
        fields = dict(line.split(":", 1) for line in record.splitlines() if len(line) > 2 and line[1] == ":")
        if "P" in fields and "V" in fields:
            versions[fields["P"]] = fields["V"]
    return versions


def check(lock, fetch_bytes=fetch, read_index=alpine_index):
    """Return (failures, moved): pins upstream no longer serves as pinned, and Alpine pins it replaced."""
    failures = []
    moved = []
    indexes = {}
    for pin in mirror.upstream_pins(lock):
        try:
            digest = hashlib.sha256(fetch_bytes(pin["url"])).hexdigest()
        except (OSError, ValueError) as error:
            failures.append(f"{pin['url']} cannot be downloaded: {error}")
            continue
        if digest != pin["sha256"]:
            failures.append(f"{pin['url']} no longer matches its pin {pin['sha256']}")
        directory = ALPINE_PACKAGE.match(pin["url"])
        if directory and "name" in pin:
            directory = directory.group(1)
            if directory not in indexes:
                try:
                    indexes[directory] = read_index(directory)
                except (OSError, ValueError, tarfile.TarError) as error:
                    failures.append(f"{directory}/APKINDEX.tar.gz cannot be read: {error}")
                    indexes[directory] = None
            current = (indexes[directory] or {}).get(pin["name"])
            if indexes[directory] is not None and current != pin["version"]:
                moved.append(f"{pin['name']} {pin['version']} -> {current or 'gone from the index'} ({directory})")
    return failures, moved


def check_mirror(lock, read_mirror):
    """Return the pins the package mirror lacks, holds with other bytes, or cannot serve.

    read_mirror(pin) returns the SHA-256 of the mirror's copy, or None when the mirror has none.
    """
    failures = []
    for pin in mirror.upstream_pins(lock):
        name = pin["url"].rsplit("/", 1)[-1]
        try:
            digest = read_mirror(pin)
        except (OSError, ValueError) as error:
            failures.append(f"the package mirror copy of {name} cannot be read: {error}")
            continue
        if digest is None:
            failures.append(f"the package mirror has no copy of {name} ({pin['sha256']})")
        elif digest != pin["sha256"]:
            failures.append(f"the package mirror copy of {name} does not match its pin {pin['sha256']}")
    return failures


def mirror_reader(environ):
    """Return (read_mirror, None) for the package mirror of this CI job, or (None, why it cannot read it)."""
    api, project, token = (environ.get(key) for key in ("CI_API_V4_URL", "CI_PROJECT_ID", "CI_JOB_TOKEN"))
    if not (api and project and token):
        return None, "CI_API_V4_URL, CI_PROJECT_ID or CI_JOB_TOKEN is unset"
    return (lambda pin: mirror.mirrored_digest(mirror.runtime.mirror_url(pin, api, project), token)), None


def main():
    lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
    pins = list(mirror.upstream_pins(lock))
    if not pins:
        print("FAILED: git-runtime-lock.json yields no upstream pins to check", file=sys.stderr)
        return 1
    failures, moved = check(lock)
    print(f"checked {len(pins)} pinned upstream files", flush=True)
    read_mirror, unreadable = mirror_reader(os.environ)
    if read_mirror:
        failures += check_mirror(lock, read_mirror)
        print(f"checked {len(pins)} package mirror copies", flush=True)
    elif os.environ.get("CI"):
        # A scheduled run that skipped the mirror would report it whole.
        failures.append(f"the package mirror was not checked: {unreadable}")
    else:
        print(f"package mirror not checked outside CI: {unreadable}", flush=True)
    for line in moved:
        print("UPSTREAM MOVED: " + line, flush=True)
    for line in failures:
        print("FAILED: " + line, file=sys.stderr)
    if failures:
        return 1
    return 2 if moved else 0


if __name__ == "__main__":
    sys.exit(main())
