"""Offline regression checks for the final publication gate."""

from pathlib import Path
import shutil
import tempfile
import unittest

from release_assets import PACKAGES, digest, verify


class ReleaseAssetsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name) / "dist"
        self.directory.mkdir()
        for name in PACKAGES:
            (self.directory / name).write_bytes(f"fixture: {name}".encode())
            self.write_checksum(name)

    def write_checksum(self, name):
        (self.directory / (name + ".sha256")).write_text(
            f"{digest(self.directory / name)}  {name}\n", encoding="ascii"
        )

    def test_complete_release(self):
        verify(self.directory)

    def test_missing_platform_blocks_publication(self):
        (self.directory / PACKAGES[0]).unlink()
        with self.assertRaisesRegex(ValueError, "missing="):
            verify(self.directory)

    def test_unexpected_asset_blocks_publication(self):
        (self.directory / "stale-build.zip").write_bytes(b"old")
        with self.assertRaisesRegex(ValueError, "unexpected="):
            verify(self.directory)

    def test_corrupt_package_blocks_publication(self):
        (self.directory / PACKAGES[1]).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "Invalid SHA-256"):
            verify(self.directory)

    def test_wrong_checksum_filename_blocks_publication(self):
        checksum = self.directory / (PACKAGES[0] + ".sha256")
        checksum.write_text(checksum.read_text().replace(PACKAGES[0], "elsewhere.exe"))
        with self.assertRaisesRegex(ValueError, "filename"):
            verify(self.directory)

    def test_empty_package_blocks_publication_even_with_matching_checksum(self):
        (self.directory / PACKAGES[0]).write_bytes(b"")
        self.write_checksum(PACKAGES[0])
        with self.assertRaisesRegex(ValueError, "nonempty regular file"):
            verify(self.directory)

    def test_matching_upload(self):
        reference = Path(self.temporary.name) / "reference"
        shutil.copytree(self.directory, reference)
        verify(self.directory, reference)

    def test_different_self_consistent_upload_blocks_publication(self):
        reference = Path(self.temporary.name) / "reference"
        shutil.copytree(self.directory, reference)
        (self.directory / PACKAGES[0]).write_bytes(b"another valid build")
        self.write_checksum(PACKAGES[0])
        with self.assertRaisesRegex(ValueError, "differs from the checked build"):
            verify(self.directory, reference)


if __name__ == "__main__":
    unittest.main()
