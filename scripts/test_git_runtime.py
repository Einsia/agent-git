import hashlib
import importlib.util
import io
import json
from pathlib import Path
import ssl
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).with_name("prepare-git-runtime.py"))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


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


if __name__ == "__main__":
    unittest.main()
