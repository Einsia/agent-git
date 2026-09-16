import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).with_name("prepare-git-runtime.py"))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


class RuntimeArchiveTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
