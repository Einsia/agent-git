#!/usr/bin/env python3
"""Install the checksum-pinned Zig wheel from the CI package mirror."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

WHEEL = "ziglang-0.16.0-py3-none-manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64.whl"
SHA256 = "9fcda73f62b851dd72a54b710ad40a209896db14cfb13649e62191243556342b"


def main():
    root = Path(".cache/ci-zig")
    root.mkdir(parents=True, exist_ok=True)
    wheel = root / WHEEL
    valid = False
    if wheel.exists():
        with wheel.open("rb") as cached:
            valid = hashlib.file_digest(cached, "sha256").hexdigest() == SHA256
    if not valid:
        mirror = f"{os.environ['CI_API_V4_URL']}/projects/{os.environ['CI_PROJECT_ID']}/packages/generic/linux-ci-tools/{SHA256}/{WHEEL}"
        config = "header = " + json.dumps("JOB-TOKEN: " + os.environ["CI_JOB_TOKEN"]) + "\n"
        subprocess.run([
            "curl", "--fail", "--silent", "--show-error", "--max-time", "180",
            "--config", "-", "--output", str(wheel), mirror,
        ], input=config, text=True, check=True, timeout=200)
        with wheel.open("rb") as downloaded:
            if hashlib.file_digest(downloaded, "sha256").hexdigest() != SHA256:
                raise RuntimeError("Zig wheel checksum mismatch")
    subprocess.run([sys.executable, "-m", "pip", "install", "--disable-pip-version-check", "--no-index", "--no-deps", str(wheel)], check=True)


if __name__ == "__main__":
    main()
