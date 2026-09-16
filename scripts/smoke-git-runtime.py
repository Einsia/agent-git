#!/usr/bin/env python3
"""Exercise Git transport and LFS from an assembled runtime without system Git."""

import argparse
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def run(root):
    windows = os.name == "nt"
    git = root / ("cmd/git.exe" if windows else "bin/git")
    exec_path = root / ("mingw64/libexec/git-core" if windows else "libexec/git-core")
    with tempfile.TemporaryDirectory(prefix="agit-runtime-smoke-") as temporary:
        home = Path(temporary).resolve()
        env = {**os.environ, "HOME": str(home), "USERPROFILE": str(home),
               "GIT_CONFIG_GLOBAL": str(home / ".gitconfig"), "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_EXEC_PATH": str(exec_path), "GIT_TERMINAL_PROMPT": "0",
               "GIT_TEMPLATE_DIR": str(root / "share/git-core/templates"),
               "PATH": os.pathsep.join(map(str, [git.parent, exec_path, root / "mingw64/bin", root / "usr/bin"]))}
        profile = b"[user]\nname = Runtime smoke\nemail = smoke@example.invalid\n"
        (home / ".gitconfig").write_bytes(profile)

        def git_run(*args, cwd=home):
            result = subprocess.run([str(git), *args], cwd=cwd, env=env, capture_output=True, text=True)
            if result.returncode:
                raise RuntimeError(f"Git {args} failed: {result.stdout}\n{result.stderr}")
            return result.stdout.strip()

        print(git_run("--version"))
        print(git_run("lfs", "version"))
        source = home / "source"
        git_run("init", "--initial-branch=main", str(source))
        git_run("lfs", "install", "--local", "--skip-repo", cwd=source)
        (source / ".gitattributes").write_text("*.bin filter=lfs diff=lfs merge=lfs -text\n")
        payload = b"LFS payload\0with exact bytes\n"
        (source / "sample.bin").write_bytes(payload)
        git_run("add", ".", cwd=source)
        git_run("-c", "commit.gpgsign=false", "commit", "-m", "Runtime smoke", cwd=source)
        assert git_run("show", "HEAD:sample.bin", cwd=source).startswith("version https://git-lfs.github.com/spec/v1")
        git_run("lfs", "fsck", cwd=source)
        remote = home / "remote.git"
        git_run("init", "--bare", "--initial-branch=main", str(remote))
        git_run("remote", "add", "origin", remote.as_uri(), cwd=source)
        git_run("lfs", "push", "origin", "main", cwd=source)
        git_run("push", "origin", "main", cwd=source)
        cloned = home / "cloned"
        git_run("clone", remote.as_uri(), str(cloned))
        git_run("lfs", "install", "--local", "--skip-repo", cwd=cloned)
        git_run("lfs", "pull", cwd=cloned)
        assert (cloned / "sample.bin").read_bytes() == payload
        assert git_run("rev-parse", "HEAD", cwd=source) == git_run("rev-parse", "HEAD", cwd=cloned)
        assert (home / ".gitconfig").read_bytes() == profile
        print("Private Git transport, LFS filtering, and profile preservation passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="agit-runtime-extract-") as temporary:
        root = Path(temporary).resolve()
        with tarfile.open(args.archive) as archive:
            for member in archive:
                path = Path(member.name)
                if path.is_absolute() or ".." in path.parts or not (member.isfile() or member.islnk()):
                    raise ValueError("unsupported runtime archive entry")
                destination = root / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                if member.islnk():
                    link = Path(member.linkname)
                    if link.is_absolute() or ".." in link.parts:
                        raise ValueError("unsafe runtime hardlink")
                    os.link(root / link, destination)
                else:
                    destination.write_bytes(archive.extractfile(member).read())
                    destination.chmod(member.mode & 0o777)
        run(root)


if __name__ == "__main__":
    main()
