#!/usr/bin/env python3
"""Assemble the corresponding source of the copyleft components in the bundled Git runtime.

git-runtime-sources.json lists the source archives a release publishes. Each names the pins in
git-runtime-lock.json it is the source of, by their SHA-256, and the pinned inputs it is built from:

- `aports` with `downloads`: an Alpine package recipe at an aports commit, file by file, and the
  upstream files it builds from. Every file the APKBUILD's sha512sums names must be among them with
  that checksum, so the archive holds exactly what Alpine compiled.
- `trees`: Git commits, archived whole. A tree with `tag` must be the commit that tag names
  upstream; a tree with `submodule` must be the commit the first tree records at that path.
- `downloads` alone: one published source package, copied verbatim, so its checksum and its
  publisher's signature still verify it.

MinGit is many packages, so git-runtime-mingit-inventory.json assigns every file of the pinned
MinGit archive to the package it comes from. A package whose license involves the GPL names its
source archive, or says why it needs none. MinGit's file list must be exactly the inventory's, so a
MinGit update that adds a file no inventoried package accounts for fails the run.

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
import urllib.parse
import urllib.request
import zipfile


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


def check_coverage(lock, sources, inventory):
    """Return copyleft_pins(lock), or raise if the source list, the MinGit inventory and the lock disagree.

    A copyleft pin or MinGit package without an archive would ship without its source, and an
    archive covering a pin the lock no longer has would publish the source of something the release
    does not ship.
    """
    required = copyleft_pins(lock)
    covered = {digest for entry in sources["sources"] for digest in entry["covers"]}
    problems = [f"no source archive covers {' '.join(required[digest])}" for digest in sorted(required.keys() - covered)]
    problems += [f"a source archive covers {digest}, which is not a copyleft pin in the lock" for digest in sorted(covered - required.keys())]
    names = [entry["asset"] for entry in sources["sources"]]
    if len(set(names)) != len(names):
        problems.append("source archive names repeat")
    # Release consumers select assets by name (npm downloads agit-*.tar.gz), so a source archive
    # stays under its own prefix. GitHub renames an asset with any other character, and the
    # Release would then not carry the name SOURCES-SHA256SUMS lists.
    problems += [f"{name} is not source- followed by letters, digits, '.', '_' and '-'" for name in names if not re.fullmatch(r"source-[A-Za-z0-9._-]+", name)]
    windows = lock["windows"]["sha256"]
    if inventory["mingit"] != windows:
        problems.append(f"the MinGit inventory is for {inventory['mingit']}, but the lock pins {windows}")
    windows_sources = {entry["asset"] for entry in sources["sources"] if windows in entry["covers"]}
    named = set()
    for package in inventory["packages"]:
        label = f"MinGit package {package['name']} {package['version']}"
        if "source" in package:
            named.add(package["source"])
            if package["source"] not in windows_sources:
                problems.append(f"{label} names {package['source']}, which is not a source archive covering MinGit")
        elif "GPL" in package["license"].upper() and "no_source" not in package:
            problems.append(f"{label} is {package['license']} but names no source archive and no reason it needs none")
    problems += [f"{asset} covers MinGit, but no MinGit package names it" for asset in sorted(windows_sources - named)]
    listed = [path for package in inventory["packages"] for path in package["files"]]
    if len(set(listed)) != len(listed):
        problems.append("the MinGit inventory assigns a file to more than one package")
    if problems:
        raise ValueError("git-runtime-sources.json does not match git-runtime-lock.json:\n  " + "\n  ".join(problems))
    return required


def check_mingit_files(lock, inventory):
    """Raise unless the pinned MinGit archive holds exactly the files the inventory assigns."""
    with zipfile.ZipFile(io.BytesIO(fetch([lock["windows"]["url"]], lock["windows"]["sha256"]))) as archive:
        shipped = {entry.filename for entry in archive.infolist() if not entry.is_dir()}
    listed = {path for package in inventory["packages"] for path in package["files"]}
    problems = [f"MinGit ships {path}, which no inventoried package accounts for" for path in sorted(shipped - listed)]
    problems += [f"the inventory lists {path}, which MinGit does not ship" for path in sorted(listed - shipped)]
    if problems:
        raise ValueError("git-runtime-mingit-inventory.json does not match the pinned MinGit:\n  " + "\n  ".join(problems))


# Hosts that did not answer during this run. One host serves many pins, so without this a host
# that is down costs every one of its downloads the full retry budget before the fallback is
# tried, and enough of them exceed the release job's timeout. Only a failure to connect or a
# timeout puts a host here: an HTTP error or wrong bytes come from a host that answers, so the
# next pin still asks it.
UNREACHABLE = set()


def get(url):
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=120) as response:
                return response.read()
        except OSError as error:
            transient = not isinstance(error, urllib.error.HTTPError) or error.code >= 500 or error.code == 429
            if not transient or attempt == 2:
                if not isinstance(error, urllib.error.HTTPError):
                    host = urllib.parse.urlsplit(url).netloc
                    UNREACHABLE.add(host)
                    print(f"{host} did not answer ({error}); skipping it for the rest of this run", file=sys.stderr)
                raise
            time.sleep(2 ** attempt)


def fetch(urls, sha256):
    """Return the bytes of the first URL whose content matches the pin."""
    failures = []
    for url in urls:
        if urllib.parse.urlsplit(url).netloc in UNREACHABLE:
            failures.append(f"{url}: skipped, its host did not answer earlier in this run")
            continue
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


def write_download(path, entry):
    (download,) = entry["downloads"]
    path.write_bytes(fetch(download["urls"], download["sha256"]))


def release_notes(written, required, inventory, mingit):
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
    lines += ["", f"These parts of {mingit} involve the GPL but ship without a source archive:", ""]
    for package in inventory["packages"]:
        if "no_source" in package:
            lines.append(f"- {package['name']} {package['version']}: {package['no_source']}.")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--notes", type=Path)
    args = parser.parse_args()
    lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
    sources = json.loads(Path(__file__).with_name("git-runtime-sources.json").read_text())
    inventory = json.loads(Path(__file__).with_name("git-runtime-mingit-inventory.json").read_text())
    required = check_coverage(lock, sources, inventory)
    check_mingit_files(lock, inventory)
    args.output.mkdir(parents=True, exist_ok=True)
    written = []
    with tempfile.TemporaryDirectory() as work:
        for entry in sources["sources"]:
            path = args.output / entry["asset"]
            if "aports" in entry:
                write_asset(path, alpine_members(entry))
            elif "trees" in entry:
                write_asset(path, tree_archives(entry, Path(work)))
            else:
                write_download(path, entry)
            written.append((hashlib.sha256(path.read_bytes()).hexdigest(), entry))
            print(f"{entry['asset']}: {path.stat().st_size} bytes", file=sys.stderr)
    (args.output / SUMS).write_text("".join(f"{digest}  {entry['asset']}\n" for digest, entry in written))
    if args.notes:
        args.notes.write_text(release_notes(written, required, inventory, required[lock["windows"]["sha256"]][0]))


if __name__ == "__main__":
    main()
