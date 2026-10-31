#!/usr/bin/env python3
"""Assemble the corresponding source of the copyleft components in the bundled Git runtime.

git-runtime-sources.json lists the source archives a release publishes. Each names the pins in
git-runtime-lock.json it is the source of, by their SHA-256, and the pinned inputs it is built from:

- `aports` with `downloads`: an Alpine package recipe at an aports commit, file by file, and the
  upstream files it builds from. Every file the APKBUILD's sha512sums names must be among them with
  that checksum, so the archive holds exactly what Alpine compiled.
- `trees`: Git commits, archived whole. A tree with `tag` must be the commit that tag names
  upstream; a tree with `submodule` must be the commit the first tree records at that path.

Every input is checked against its pin, and a copyleft runtime pin that no archive covers fails the
run, so a release that runs this cannot publish a runtime without its source. The archives are
written to the output directory with SOURCES-SHA256SUMS; --notes also writes the release notes
section that lists them.
"""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request


SUMS = "SOURCES-SHA256SUMS"
ALPINE_RECIPE_URLS = (
    "https://gitlab.alpinelinux.org/alpine/aports/-/raw/{commit}/{path}",
    "https://raw.githubusercontent.com/alpinelinux/aports/{commit}/{path}",
)


def copyleft_pins(lock):
    """Return {sha256: (component, platform)} for every runtime pin whose source must be published.

    The macOS and Windows archives carry Git, which is GPL-2.0-only. A Linux package needs its
    source unless one alternative of its license expression is not copyleft, as with zstd's
    "BSD-3-Clause OR GPL-2.0-or-later".
    """
    pins = {}
    for arch, pin in lock["macos"].items():
        pins[pin["sha256"]] = (f"Git {pin['version']} (dugite-native)", f"macOS {arch}")
    pins[lock["windows"]["sha256"]] = (f"MinGit {lock['windows']['version']}", "Windows x64")
    for arch, packages in lock["linux"].items():
        for pin in packages:
            if all("GPL" in choice for choice in pin["license"].split(" OR ")):
                pins[pin["sha256"]] = (f"{pin['name']} {pin['version']}", f"Linux {arch}")
    return pins


def check_coverage(lock, sources):
    """Return copyleft_pins(lock), or raise if the source list and the lock disagree.

    A copyleft pin without an archive would ship without its source, and an archive covering a pin
    the lock no longer has would publish the source of something the release does not ship.
    """
    required = copyleft_pins(lock)
    covered = {digest for entry in sources["sources"] for digest in entry["covers"]}
    problems = [f"no source archive covers {' '.join(required[digest])}" for digest in sorted(required.keys() - covered)]
    problems += [f"a source archive covers {digest}, which is not a copyleft pin in the lock" for digest in sorted(covered - required.keys())]
    names = [entry["asset"] for entry in sources["sources"]]
    if len(set(names)) != len(names):
        problems.append("source archive names repeat")
    # Release consumers select assets by name (npm downloads agit-*.tar.gz), so a source archive
    # stays under its own prefix and never takes another asset's name.
    problems += [f"{name} does not start with source-" for name in names if not name.startswith("source-")]
    if problems:
        raise ValueError("git-runtime-sources.json does not match git-runtime-lock.json:\n  " + "\n  ".join(problems))
    return required


def get(url):
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=120) as response:
                return response.read()
        except OSError as error:
            transient = not isinstance(error, urllib.error.HTTPError) or error.code >= 500 or error.code == 429
            if not transient or attempt == 2:
                raise
            time.sleep(2 ** attempt)


def fetch(urls, sha256):
    """Return the bytes of the first URL whose content matches the pin."""
    failures = []
    for url in urls:
        try:
            body = get(url)
        except OSError as error:
            failures.append(f"{url}: {error}")
            continue
        if hashlib.sha256(body).hexdigest() == sha256:
            return body
        failures.append(f"{url}: does not match its pinned checksum")
    raise ValueError("no pinned copy:\n  " + "\n  ".join(failures))


def apkbuild_checksums(apkbuild):
    """Return {file name: sha512} from an APKBUILD's sha512sums."""
    match = re.search(r"^sha512sums=([\"'])(.*?)\1", apkbuild.decode(), re.MULTILINE | re.DOTALL)
    if not match:
        raise ValueError("the APKBUILD has no sha512sums")
    return {name: digest for digest, name in (line.split(maxsplit=1) for line in match.group(2).splitlines() if line.strip())}


def alpine_members(entry):
    recipe = entry["aports"]
    files = {}
    for name, sha256 in recipe["files"].items():
        path = f"{recipe['directory']}/{name}"
        files[name] = fetch([url.format(commit=recipe["commit"], path=path) for url in ALPINE_RECIPE_URLS], sha256)
    for download in entry["downloads"]:
        files[download["name"]] = fetch(download["urls"], download["sha256"])
    checksums = apkbuild_checksums(files["APKBUILD"])
    for name, sha512 in checksums.items():
        if name not in files:
            raise ValueError(f"{entry['asset']}: the APKBUILD builds from {name}, which is not pinned")
        if hashlib.sha512(files[name]).hexdigest() != sha512:
            raise ValueError(f"{entry['asset']}: {name} is not the file the APKBUILD builds from")
    for download in entry["downloads"]:
        if download["name"] not in checksums:
            raise ValueError(f"{entry['asset']}: the APKBUILD does not build from {download['name']}")
    root = entry["asset"].removesuffix(".tar") + "/" + recipe["directory"]
    for name, body in sorted(files.items()):
        header = tarfile.TarInfo(f"{root}/{name}")
        header.mode = 0o644
        header.size = len(body)
        yield header, body


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True).stdout.decode()


def tree_archives(entry, work):
    archives = []
    first = None
    for index, tree in enumerate(entry["trees"]):
        repository = work / f"{entry['asset']}-{index}"
        git("init", "-q", "--bare", str(repository))
        # The archive is the commit's tree as it is: no file is left out and no keyword expanded.
        (repository / "info" / "attributes").write_text("* -export-ignore -export-subst\n")
        git("fetch", "-q", "--depth=1", tree["repository"], tree["commit"], cwd=repository)
        if git("rev-parse", "FETCH_HEAD^{commit}", cwd=repository).strip() != tree["commit"]:
            raise ValueError(f"{tree['repository']} did not return {tree['commit']}")
        if "tag" in tree:
            ref = "refs/tags/" + tree["tag"]
            refs = dict(reversed(line.split("\t")) for line in git("ls-remote", tree["repository"], ref, ref + "^{}").splitlines())
            if refs.get(ref + "^{}", refs.get(ref)) != tree["commit"]:
                raise ValueError(f"{tree['repository']} tag {tree['tag']} is not {tree['commit']}")
        if "submodule" in tree:
            recorded = git("ls-tree", first["commit"], "--", tree["submodule"], cwd=first["repository"]).split()
            if recorded[:3] != ["160000", "commit", tree["commit"]]:
                raise ValueError(f"{first['commit']} does not record {tree['commit']} at {tree['submodule']}")
        archives.append(subprocess.run(
            ["git", "archive", "--format=tar", f"--prefix={tree['prefix']}/", tree["commit"]],
            cwd=repository, check=True, capture_output=True).stdout)
        first = first or {"repository": repository, "commit": tree["commit"]}
    for archive in archives:
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as members:
            for header in members:
                yield header, members.extractfile(header).read() if header.isfile() else None


def write_asset(path, members):
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        partial = Path(temporary.name)
    try:
        with partial.open("wb") as raw:
            stream = gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) if path.name.endswith(".gz") else raw
            with tarfile.open(fileobj=stream, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for header, body in members:
                    archive.addfile(header, None if body is None else io.BytesIO(body))
            if stream is not raw:
                stream.close()
        os.replace(partial, path)
    finally:
        partial.unlink(missing_ok=True)


def release_notes(written, required):
    lines = [
        "### Source code of the bundled Git runtime",
        "",
        "agit bundles a Git runtime that includes GPL- and LGPL-licensed components. These assets"
        " hold their corresponding source, built from the same pinned inputs as this release's"
        f" binaries. `{SUMS}` lists the same checksums.",
        "",
        "| Asset | Source of | SHA-256 |",
        "| --- | --- | --- |",
    ]
    for digest, entry in written:
        components = sorted({required[pin][0] for pin in entry["covers"]})
        platforms = sorted({required[pin][1] for pin in entry["covers"]})
        what = ", ".join(components) + " (" + ", ".join(platforms) + ")"
        if "component" in entry:
            what = f"{entry['component']}, in {what}"
        lines.append(f"| `{entry['asset']}` | {what} | `{digest}` |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--notes", type=Path)
    args = parser.parse_args()
    lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
    sources = json.loads(Path(__file__).with_name("git-runtime-sources.json").read_text())
    required = check_coverage(lock, sources)
    args.output.mkdir(parents=True, exist_ok=True)
    written = []
    with tempfile.TemporaryDirectory() as work:
        for entry in sources["sources"]:
            path = args.output / entry["asset"]
            members = alpine_members(entry) if "aports" in entry else tree_archives(entry, Path(work))
            write_asset(path, members)
            written.append((hashlib.sha256(path.read_bytes()).hexdigest(), entry))
            print(f"{entry['asset']}: {path.stat().st_size} bytes", file=sys.stderr)
    (args.output / SUMS).write_text("".join(f"{digest}  {entry['asset']}\n" for digest, entry in written))
    if args.notes:
        args.notes.write_text(release_notes(written, required))


if __name__ == "__main__":
    main()
