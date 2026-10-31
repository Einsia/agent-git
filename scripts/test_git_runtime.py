import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import ssl
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import zipfile

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).with_name("prepare-git-runtime.py"))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)
spec = importlib.util.spec_from_file_location("mirror", Path(__file__).with_name("mirror-git-runtime-archives.py"))
mirror = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mirror)
spec = importlib.util.spec_from_file_location("sources", Path(__file__).with_name("git-runtime-sources.py"))
sources = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sources)


class RuntimeArchiveTests(unittest.TestCase):
    def setUp(self):
        # The upstream-path contracts below hold whether or not the suite runs inside GitLab CI.
        environment = patch.dict(runtime.os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        for key in ("CI_API_V4_URL", "CI_PROJECT_ID", "CI_JOB_TOKEN"):
            runtime.os.environ.pop(key, None)

    def test_linux_certificate_store_is_pinned_and_available_without_network(self):
        lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            runtime.urllib.request, "urlopen", side_effect=AssertionError("network unavailable")
        ):
            body = runtime.download(lock["linux_ca_bundle"], Path(temporary))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.load_verify_locations(cadata=body.decode())
        self.assertGreater(context.cert_store_stats()["x509_ca"], 0)

    def test_vendored_license_checkout_line_endings_cannot_change_pinned_payload(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "license.txt"
            body = b"License text\nAll rights reserved.\n"
            pin = {"path": str(path), "sha256": hashlib.sha256(body).hexdigest()}
            path.write_bytes(body.replace(b"\n", b"\r\n"))
            self.assertEqual(runtime.download(pin, Path(temporary)), body)
            path.write_bytes(b"Altered license\r\n")
            with self.assertRaisesRegex(ValueError, "vendored checksum mismatch"):
                runtime.download(pin, Path(temporary))

    def test_reproducible_archive_preserves_executable_aliases_without_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {"bin/git": (b"git executable", 0o755), "libexec/git-core/git": (b"git executable", 0o755)}
            runtime.write_archive(files, root / "first.tar.gz")
            runtime.write_archive(dict(reversed(list(files.items()))), root / "second.tar.gz")
            self.assertEqual((root / "first.tar.gz").read_bytes(), (root / "second.tar.gz").read_bytes())
            with tarfile.open(root / "first.tar.gz") as archive:
                alias = archive.getmember("libexec/git-core/git")
                self.assertTrue(alias.islnk())
                self.assertEqual(alias.linkname, "bin/git")
                self.assertEqual(archive.extractfile(alias).read(), b"git executable")

    def test_corrupt_cached_and_downloaded_bytes_never_become_a_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            cache = Path(temporary)
            digest = hashlib.sha256(b"expected").hexdigest()
            (cache / (digest + "-runtime.tar.gz")).write_bytes(b"bad cache")
            pin = {"url": "https://example.invalid/runtime.tar.gz", "sha256": digest}
            with patch.object(runtime.urllib.request, "urlopen", return_value=io.BytesIO(b"bad download")):
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    runtime.download(pin, cache)

    def test_archive_links_cannot_escape_the_payload(self):
        with self.assertRaisesRegex(ValueError, "unsafe archive path"):
            runtime.resolve_links({}, {"bin/git": "../../outside"})

    def test_transient_download_failure_can_only_publish_verified_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            body = b"verified runtime"
            pin = {"url": "https://example.invalid/runtime.tar.gz", "sha256": hashlib.sha256(body).hexdigest()}
            with patch.object(runtime.urllib.request, "urlopen", side_effect=[TimeoutError(), io.BytesIO(body)]), patch.object(runtime.time, "sleep"):
                self.assertEqual(runtime.download(pin, Path(temporary)), body)
            cached = Path(temporary) / (pin["sha256"] + "-runtime.tar.gz")
            self.assertEqual(cached.read_bytes(), body)

    def test_package_mirror_is_a_shortcut_that_cannot_change_the_pinned_bytes(self):
        """In CI the package registry copy is tried first and used only if it has the pinned digest.

        A mirror that served unchecked bytes, or a missing copy that failed the job instead of falling
        back to the upstream URL, would each break one of the three cases below.
        """
        body = b"verified runtime"
        pin = {"url": "https://example.invalid/runtime.tar.gz", "sha256": hashlib.sha256(body).hexdigest()}
        api = "https://gitlab.invalid/api/v4"
        mirror = f"{api}/projects/87/packages/generic/git-runtime-archives/{pin['sha256']}/runtime.tar.gz"
        cases = {
            "verified copy": (lambda: io.BytesIO(body), 0),
            "tampered copy": (lambda: io.BytesIO(b"tampered"), 1),
            "missing copy": (lambda: (_ for _ in ()).throw(urllib.error.HTTPError(mirror, 404, "Not Found", {}, None)), 1),
        }
        for case, (mirror_reply, upstream_requests) in cases.items():
            requested = []

            def urlopen(request, timeout):
                url = getattr(request, "full_url", request)
                requested.append(url)
                if url == mirror:
                    self.assertEqual(request.get_header("Job-token"), "job-token")
                    return mirror_reply()
                return io.BytesIO(body)

            environment = {"CI_API_V4_URL": api, "CI_PROJECT_ID": "87", "CI_JOB_TOKEN": "job-token"}
            with self.subTest(case), tempfile.TemporaryDirectory() as temporary, patch.dict(runtime.os.environ, environment):
                with patch.object(runtime.urllib.request, "urlopen", side_effect=urlopen):
                    self.assertEqual(runtime.download(pin, Path(temporary)), body)
                self.assertEqual(requested[0], mirror)
                self.assertEqual(requested.count(pin["url"]), upstream_requests)

    def test_package_mirror_receives_only_verified_copies_of_missing_pins(self):
        """The upload job adds an archive only when the mirror lacks it and upstream serves the pinned bytes.

        The walk must yield each downloaded pin once and nothing a vendored pin carries, or the job
        uploads duplicates and goes red over provenance archives no build fetches. An uploader that
        trusted upstream would publish tampered bytes; one that ignored a wrong mirror copy, took an
        unavailable mirror for a missing copy, skipped the read-back, or let a refused upload end the
        run would report success or stop with pins unchecked; each breaks a case below.
        """
        body = b"verified runtime"
        pin = {"url": "https://example.invalid/runtime.tar.gz", "sha256": hashlib.sha256(body).hexdigest()}
        provenance = {"url": "https://example.invalid/bundle.apk", "sha256": "1" * 64}
        vendored = {"path": "git-runtime-data/bundle.pem.gz", "sha256": "0" * 64, "source_archives": [provenance]}
        lock = {"linux": [pin, dict(pin)], "linux_ca_bundle": vendored}
        self.assertEqual(list(mirror.upstream_pins(lock)), [pin])
        api = "https://gitlab.invalid/api/v4"
        copy_url = f"{api}/projects/87/packages/generic/git-runtime-archives/{pin['sha256']}/runtime.tar.gz"
        server_error = urllib.error.HTTPError(copy_url, 500, "Internal Server Error", {}, None)
        refused = urllib.error.HTTPError(copy_url, 403, "Forbidden", {}, None)
        cases = {
            # mirror copy before the run, upstream bytes, mirror read error, upload error, upload kept,
            # whether the pinned bytes are sent, whether the run reports a failure
            "copy present": (body, body, None, None, True, False, False),
            "copy missing": (None, body, None, None, True, True, False),
            "upstream tampered": (None, b"tampered", None, None, True, False, True),
            "copy tampered": (b"tampered", body, None, None, True, False, True),
            "mirror unavailable": (None, body, server_error, None, True, False, True),
            "upload refused": (None, body, None, refused, True, True, True),
            "upload lost": (None, body, None, None, False, True, True),
        }
        for case, (copy, upstream, read_error, upload_error, kept, uploaded, failed) in cases.items():
            store = {} if copy is None else {copy_url: copy}
            puts = []

            def urlopen(request, timeout):
                url = getattr(request, "full_url", request)
                if url == copy_url:
                    self.assertEqual(request.get_header("Job-token"), "job-token")
                    if request.get_method() == "PUT":
                        puts.append(request.data)
                        if upload_error:
                            raise upload_error
                        if kept:
                            store[copy_url] = request.data
                        return io.BytesIO(b"{}")
                    if read_error:
                        raise read_error
                    if copy_url not in store:
                        raise urllib.error.HTTPError(copy_url, 404, "Not Found", {}, None)
                    return io.BytesIO(store[copy_url])
                self.assertEqual(url, pin["url"])
                return io.BytesIO(upstream)

            with self.subTest(case), tempfile.TemporaryDirectory() as temporary:
                with patch.object(runtime.urllib.request, "urlopen", side_effect=urlopen), patch.object(mirror.time, "sleep"):
                    failures = mirror.mirror_missing(lock, api, "87", "job-token", Path(temporary))
                self.assertEqual(puts, [body] if uploaded else [])
                self.assertEqual(bool(failures), failed, failures)


class RuntimeSourceTests(unittest.TestCase):
    def test_every_copyleft_runtime_pin_has_a_published_source(self):
        """A lock change that ships a GPL or LGPL archive must name the archive holding its source.

        The release refuses to publish on the same check, so this reports the gap with the pin
        change instead. A list that dropped a pin, or still covered one the lock replaced, must be
        refused too, or a stale list would pass.
        """
        lock = json.loads(Path(__file__).with_name("git-runtime-lock.json").read_text())
        listed = json.loads(Path(__file__).with_name("git-runtime-sources.json").read_text())
        inventory = json.loads(Path(__file__).with_name("git-runtime-mingit-inventory.json").read_text())
        sources.check_coverage(lock, listed, inventory)
        dropped = json.loads(json.dumps(listed))
        dropped["sources"][0]["covers"].pop()
        with self.assertRaisesRegex(ValueError, "no source archive covers"):
            sources.check_coverage(lock, dropped, inventory)
        stale = json.loads(json.dumps(listed))
        stale["sources"][0]["covers"].append("0" * 64)
        with self.assertRaisesRegex(ValueError, "not a copyleft pin"):
            sources.check_coverage(lock, stale, inventory)
        unsourced = json.loads(json.dumps(inventory))
        package = next(package for package in unsourced["packages"] if "source" in package)
        del package["source"]
        with self.assertRaisesRegex(ValueError, re.escape(package["name"]) + " .* names no source archive"):
            sources.check_coverage(lock, listed, unsourced)

    def test_a_mingit_file_no_inventoried_package_accounts_for_stops_the_release(self):
        """MinGit's file list must be exactly the inventory's, in both directions.

        A MinGit update that adds a GPL binary would otherwise ship it without its source, and an
        inventory listing files MinGit no longer has would vouch for a build it was not made from.
        """
        def mingit(paths):
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, "w") as archive:
                for path in paths:
                    archive.writestr(path, b"content")
            return buffer.getvalue()

        inventory = {"packages": [{"name": "sed", "files": ["usr/bin/sed.exe"]}, {"name": "git", "files": ["cmd/git.exe"]}]}
        cases = {
            "exact": (["cmd/git.exe", "usr/bin/sed.exe", "usr/bin/"], None),
            "added file": (["cmd/git.exe", "usr/bin/sed.exe", "usr/bin/grep.exe"], "ships usr/bin/grep.exe, which no inventoried package"),
            "dropped file": (["cmd/git.exe"], "lists usr/bin/sed.exe, which MinGit does not ship"),
        }
        for case, (paths, error) in cases.items():
            body = mingit(paths)
            lock = {"windows": {"url": "https://example.invalid/MinGit.zip", "sha256": hashlib.sha256(body).hexdigest()}}
            with self.subTest(case), patch.object(sources.urllib.request, "urlopen", return_value=io.BytesIO(body)):
                if error:
                    with self.assertRaisesRegex(ValueError, error):
                        sources.check_mingit_files(lock, inventory)
                else:
                    sources.check_mingit_files(lock, inventory)

    def test_alpine_source_archive_holds_what_the_apkbuild_builds_from(self):
        """Every file the APKBUILD's sha512sums names is in the archive, with that checksum.

        Checking only the pins in git-runtime-sources.json would let a forgotten patch, or an upstream
        tarball Alpine did not compile, be published as the source.
        """
        tarball, patch_file = b"upstream source", b"--- a\n+++ b\n"
        apkbuild = (
            f'pkgname=x\nsha512sums="\n{hashlib.sha512(tarball).hexdigest()}  x-1.tar.gz\n'
            f'{hashlib.sha512(patch_file).hexdigest()}  fix.patch\n"\n'
        ).encode()
        recipe = {"APKBUILD": apkbuild, "fix.patch": patch_file}
        cases = {
            "complete": (["APKBUILD", "fix.patch"], tarball, None),
            "recipe file left out": (["APKBUILD"], tarball, "builds from fix.patch, which is not pinned"),
            "another tarball": (["APKBUILD", "fix.patch"], b"other source", "x-1.tar.gz is not the file"),
        }
        for case, (pinned, download, error) in cases.items():
            served = dict(recipe, **{"x-1.tar.gz": download})
            entry = {
                "asset": "source-alpine-x-1-r0.tar",
                "aports": {"commit": "c" * 40, "directory": "main/x", "files": {name: hashlib.sha256(recipe[name]).hexdigest() for name in pinned}},
                "downloads": [{"name": "x-1.tar.gz", "urls": ["https://example.invalid/x-1.tar.gz"], "sha256": hashlib.sha256(download).hexdigest()}],
            }
            with self.subTest(case), patch.object(
                sources.urllib.request, "urlopen", side_effect=lambda url, timeout: io.BytesIO(served[url.rsplit("/", 1)[1]])
            ):
                if error:
                    with self.assertRaisesRegex(ValueError, error):
                        list(sources.alpine_members(entry))
                else:
                    members = {header.name: body for header, body in sources.alpine_members(entry)}
                    self.assertEqual(members, {f"source-alpine-x-1-r0/main/x/{name}": body for name, body in served.items()})


if __name__ == "__main__":
    unittest.main()
