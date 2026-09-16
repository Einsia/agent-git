#!/usr/bin/env python3
"""Assemble checksum-pinned, relocatable Git and Git LFS archives for CLI builds."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import stat
import struct
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile


def download(pin, cache):
    if "path" in pin:
        # Git checkout filters can change text line endings; the pin verifies canonical upstream bytes.
        body = Path(__file__).parent.joinpath(pin["path"]).read_bytes().replace(b"\r\n", b"\n")
        if hashlib.sha256(body).hexdigest() != pin["sha256"]:
            raise ValueError("vendored checksum mismatch: " + pin["path"])
        return body
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / (pin["sha256"] + "-" + pin["url"].rsplit("/", 1)[-1])
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != pin["sha256"]:
        for attempt in range(3):
            print(f"Downloading {pin['url']} (attempt {attempt + 1})", file=sys.stderr, flush=True)
            try:
                with urllib.request.urlopen(pin["url"], timeout=120) as response:
                    body = response.read()
                break
            except OSError as error:
                permanent = isinstance(error, urllib.error.HTTPError) and 400 <= error.code < 500 and error.code != 429
                if permanent or attempt == 2:
                    raise RuntimeError("could not download " + pin["url"]) from error
                time.sleep(2 ** attempt)
        if hashlib.sha256(body).hexdigest() != pin["sha256"]:
            raise ValueError("upstream checksum mismatch: " + pin["url"])
        path.write_bytes(body)
    return path.read_bytes()


def safe_path(name):
    name = name.removeprefix("./")
    if not name or name.startswith("/") or ".." in PurePosixPath(name).parts or "\\" in name:
        raise ValueError("unsafe archive path: " + name)
    return name


def read_archive(body):
    files = {}
    links = {}
    if body.startswith(b"PK"):
        with zipfile.ZipFile(io.BytesIO(body)) as archive:
            for entry in archive.infolist():
                if entry.is_dir():
                    files[safe_path(entry.filename.rstrip("/"))] = None
                    continue
                name = safe_path(entry.filename)
                mode = entry.external_attr >> 16
                if stat.S_ISLNK(mode):
                    links[name] = posixpath.normpath(posixpath.join(posixpath.dirname(name), archive.read(entry).decode()))
                else:
                    files[name] = (archive.read(entry), 0o755 if name.endswith((".exe", ".dll")) or mode & 0o111 else 0o644)
    else:
        # APK signatures, control metadata, and payloads are concatenated tar streams.
        with tarfile.open(fileobj=io.BytesIO(body), mode="r:*", ignore_zeros=True) as archive:
            for entry in archive:
                if entry.isdir():
                    if entry.name.strip("./"):
                        files[safe_path(entry.name.rstrip("/"))] = None
                    continue
                name = safe_path(entry.name)
                if entry.isfile():
                    files[name] = (archive.extractfile(entry).read(), 0o755 if entry.mode & 0o111 else 0o644)
                elif entry.issym() or entry.islnk():
                    base = posixpath.dirname(name) if entry.issym() else ""
                    links[name] = posixpath.normpath(posixpath.join(base, entry.linkname))
                else:
                    raise ValueError("unsupported archive entry: " + name)
    return files, links


def resolve_links(files, links):
    def resolve(name, seen):
        if name in seen:
            raise ValueError("cyclic archive link: " + name)
        if name in files:
            return files[name]
        if name not in links:
            raise ValueError("unresolved archive link: " + name)
        return resolve(safe_path(links[name].lstrip("/")), seen | {name})

    for name in links:
        files[name] = resolve(name, set())
    return {name: body for name, body in files.items() if body is not None}


def linux_runtime(packages, cache, architecture):
    files, links = {}, {}
    for package in packages:
        package_files, package_links = read_archive(download(package, cache))
        files.update(package_files)
        links.update(package_links)
    files = resolve_links(files, links)
    result = {}
    for name, content in files.items():
        if name.startswith(("lib/", "usr/lib/")):
            result["native/" + name] = content
        elif name.startswith("usr/share/git-core/"):
            result[name.removeprefix("usr/")] = content
        elif name == "etc/ssl/cert.pem" or name == "etc/ssl/certs/ca-certificates.crt":
            result["ssl/cert.pem"] = content
        elif name.startswith(("usr/bin/git", "usr/libexec/git-core/")):
            destination = name.removeprefix("usr/")
            if content[0].startswith(b"\x7fELF"):
                result["native/" + name] = content
                relative_root = posixpath.relpath(".", posixpath.dirname(destination))
                wrapper = (
                    "#!/bin/sh\n"
                    f'root="${{0%/*}}/{relative_root}"\n'
                    f'exec "$root/native/lib/ld-musl-{architecture}.so.1" '
                    '--library-path "$root/native/lib:$root/native/usr/lib" '
                    f'"$root/native/{name}" "$@"\n'
                )
                result[destination] = (wrapper.encode(), 0o755)
            else:
                result[destination] = content
    return result


def assemble(lock, target, cache):
    windows = target == "x86_64-pc-windows-msvc"
    if "apple-darwin" in target:
        arch = "arm64" if target.startswith("aarch64-") else "x64"
        git = lock["macos"][arch]
        files, links = read_archive(download(git, cache))
        files = resolve_links(files, links)
        files = {
            name: value for name, value in files.items()
            if name == "bin/git" or name.startswith("share/git-core/")
            or (name.startswith("libexec/git-core/") and (
                name.rsplit("/", 1)[-1].startswith("git") or "/mergetools/" in name
            ) and not name.rsplit("/", 1)[-1].startswith("git-credential-manager"))
        }
        lfs_key = "darwin-arm64" if arch == "arm64" else "darwin-amd64"
    elif "linux" in target:
        arch = "aarch64" if target.startswith("aarch64-") else "x86_64"
        git = lock["linux"][arch]
        files = linux_runtime(git, cache, arch)
        lfs_key = "linux-arm64" if arch == "aarch64" else "linux-amd64"
    elif windows:
        git = lock["windows"]
        files, links = read_archive(download(git, cache))
        files = resolve_links(files, links)
        lfs_key = "windows-amd64"
    else:
        raise ValueError("unsupported Git runtime target: " + target)
    if not windows:
        files["bin/sh"] = (b'#!/bin/sh\nexec /bin/sh "$@"\n', 0o755)
    lfs = lock["lfs"][lfs_key]
    lfs_files, lfs_links = read_archive(download(lfs, cache))
    lfs_files = resolve_links(lfs_files, lfs_links)
    executable = "git-lfs.exe" if windows else "git-lfs"
    exec_path = "mingw64/libexec/git-core/" if windows else "libexec/git-core/"
    candidates = [value for name, value in lfs_files.items() if name.rsplit("/", 1)[-1] == executable]
    if len(candidates) != 1:
        raise ValueError("LFS archive must contain its platform executable")
    files[exec_path + executable] = (candidates[0][0], 0o755)
    git_path = "cmd/git.exe" if windows else "native/usr/bin/git" if "linux" in target else "bin/git"
    for body in (files[git_path][0], candidates[0][0]):
        arm = target.startswith("aarch64-")
        if body.startswith(b"\x7fELF"):
            valid = struct.unpack_from("<H", body, 18)[0] == (183 if arm else 62)
        elif body.startswith(b"\xcf\xfa\xed\xfe"):
            valid = struct.unpack_from("<I", body, 4)[0] == (0x100000c if arm else 0x1000007)
        elif body.startswith(b"MZ"):
            pe = struct.unpack_from("<I", body, 60)[0]
            valid = body[pe:pe + 4] == b"PE\x00\x00" and struct.unpack_from("<H", body, pe + 4)[0] == 0x8664
        else:
            valid = False
        if not valid:
            raise ValueError("Git or Git LFS architecture does not match " + target)
    for name, content in lfs_files.items():
        if name.rsplit("/", 1)[-1].lower().startswith(("license", "copying")):
            files["licenses/git-lfs/" + name.rsplit("/", 1)[-1]] = content
    for name, pin in lock["licenses"].items():
        if "linux" in target or name in ("GPL-2.0-only", "MIT"):
            files["licenses/" + name + ".txt"] = (download(pin, cache), 0o644)
    manifest = {"target": target, "git": git, "lfs": lfs}
    files["runtime-manifest.json"] = (json.dumps(manifest, indent=2).encode() + b"\n", 0o644)
    files["THIRD-PARTY-NOTICES.txt"] = (
        b"Git and Git LFS run as separate programs and retain their upstream licenses.\n"
        b"Git: GPL-2.0-only; Git LFS: MIT. This runtime does not install global Git configuration.\n"
        b"runtime-manifest.json records immutable upstream versions, package checksums, and Linux source recipes.\n"
        b"Git sources: https://github.com/git/git/tags\n"
        b"Git for Windows sources: https://github.com/git-for-windows/git/tags\n"
        b"Git LFS sources: https://github.com/git-lfs/git-lfs/tags\n", 0o644)
    return files


def write_archive(files, output):
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as temporary:
        path = Path(temporary.name)
    try:
        with path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=6) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                seen = {}
                for name, (body, mode) in sorted(files.items()):
                    safe_path(name)
                    header = tarfile.TarInfo(name)
                    header.mode = mode
                    key = (hashlib.sha256(body).digest(), mode)
                    if key in seen:
                        header.type = tarfile.LNKTYPE
                        header.linkname = seen[key]
                        archive.addfile(header)
                    else:
                        seen[key] = name
                        header.size = len(body)
                        archive.addfile(header, io.BytesIO(body))
        os.replace(path, output)
    finally:
        path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target")
    parser.add_argument("output", type=Path)
    parser.add_argument("--cache", type=Path, default=Path(".cache/git-runtime"))
    args = parser.parse_args()
    lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
    files = assemble(lock, args.target, args.cache)
    write_archive(files, args.output)
    args.output.with_suffix(args.output.suffix + ".target").write_text(args.target + "\n")
    print(f"Git runtime: {args.output} ({args.output.stat().st_size} compressed bytes)")


if __name__ == "__main__":
    main()
