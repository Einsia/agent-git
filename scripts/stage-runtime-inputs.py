#!/usr/bin/env python3
"""Stage the assets of the runtime-inputs Release for the lock at HEAD.

The GitHub release build reads every pinned runtime archive from a public Release that holds what
upstream served when the lock was written, so upstream removing a pinned file cannot fail a release.
This writes that Release's assets into one directory and nothing else:

- every upstream pin of git-runtime-lock.json, downloaded and checked against its SHA-256, under
  prepare-git-runtime.py's asset name; a pin that cannot be downloaded or does not match stops the
  run before anything is staged;
- the corresponding source archives and SOURCES-SHA256SUMS, written by git-runtime-sources.py into
  the same directory, exactly as the product Release builds them.

It prints the Release's tag on its last line. The directory holds no log, cache or notes file.
"""

import argparse
import hashlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).with_name("prepare-git-runtime.py"))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


def release_pins(lock):
    """Yield every upstream pin of the lock, each architecture's separately.

    A pin with a vendored path never reaches the network. Builds look each pin up by its asset
    name, and Alpine's architectures carry identical files under different names, so a walk that
    kept one pin per digest would leave an architecture without its asset.
    """
    pending = [lock]
    while pending:
        node = pending.pop()
        if isinstance(node, dict):
            if "path" in node:
                continue
            if "url" in node and "sha256" in node:
                yield node
            pending.extend(node.values())
        elif isinstance(node, list):
            pending.extend(node)


def stage_pins(lock, output, cache, download=runtime.download):
    """Write every upstream pin into output under its asset name, and return the names written."""
    staged = {}
    for pin in release_pins(lock):
        name = runtime.asset_name(pin)
        if name in staged:
            if staged[name] != pin["sha256"]:
                raise ValueError("two pins with different checksums share the asset name " + name)
            continue
        body = download(pin, cache)
        if hashlib.sha256(body).hexdigest() != pin["sha256"]:
            raise ValueError("pinned download does not match its checksum: " + pin["url"])
        (output / name).write_bytes(body)
        staged[name] = pin["sha256"]
    return list(staged)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    lock_path = Path(__file__).with_name("git-runtime-lock.json")
    lock = runtime.json.loads(lock_path.read_text())
    args.output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory() as cache:
        names = stage_pins(lock, args.output, Path(cache))
    if not names:
        sys.exit("git-runtime-lock.json yields no upstream pins to stage")
    subprocess.run([sys.executable, str(Path(__file__).with_name("git-runtime-sources.py")), str(args.output)], check=True)
    print(f"staged {len(names)} pinned inputs and their sources", flush=True)
    print(runtime.inputs_tag(lock_path.read_bytes()))


if __name__ == "__main__":
    main()
