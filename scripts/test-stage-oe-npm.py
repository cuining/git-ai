import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).with_name("stage-oe-npm.py")
PACKER_SPEC = importlib.util.spec_from_file_location("pack_npm", SCRIPT.with_name("pack-npm.py"))
PACKER = importlib.util.module_from_spec(PACKER_SPEC)
PACKER_SPEC.loader.exec_module(PACKER)
ROOT = SCRIPT.resolve().parents[1]
SOURCE_COMMIT = "a" * 40
BINARY = b"native binary fixture\x00"
PLATFORMS = {"darwin-arm64", "darwin-x64", "linux-arm64-musl", "linux-x64-musl", "win32-x64"}


class OeNpmStagingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory(prefix="git-ai-oe-fixtures-")
        cls.addClassCleanup(temporary.cleanup)
        root = Path(temporary.name)
        binary = root / "dcc-ai-tracking"
        binary.write_bytes(BINARY)
        cls.version = PACKER.cargo_version(ROOT)
        metadata = root / "cargo-metadata.json"
        metadata.write_text(json.dumps({"packages": [{"name": "git-ai", "version": cls.version}]}))
        cls.archives = {}
        for platform in sorted(PLATFORMS):
            archive = PACKER.pack(ROOT, binary, platform, root, SOURCE_COMMIT, metadata)
            cls.archives[archive.name] = archive.read_bytes()

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="git-ai-oe-stage-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.bundle = root / "packages.zip"
        self.output = root / "batch"

    def stage(self, archives=None, sha256=None, source_commit=SOURCE_COMMIT):
        with zipfile.ZipFile(self.bundle, "w") as bundle:
            for filename, data in (self.archives if archives is None else archives).items():
                bundle.writestr(filename, data)
        return subprocess.run([
            sys.executable, str(SCRIPT), "--bundle", str(self.bundle),
            "--sha256", sha256 or hashlib.sha256(self.bundle.read_bytes()).hexdigest(),
            "--source-commit", source_commit, "--output", str(self.output),
        ], text=True, capture_output=True, timeout=10)

    def test_stages_five_platform_packages_in_oe_batch_directory(self):
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({path.name for path in self.output.iterdir()}, PLATFORMS)
        for platform in PLATFORMS:
            with self.subTest(platform=platform):
                package = self.output / platform
                windows = platform == "win32-x64"
                binary = package / "bin" / ("dcc-ai-tracking.exe" if windows else "dcc-ai-tracking")
                self.assertEqual(binary.read_bytes(), BINARY)
                self.assertTrue(os.access(binary, os.X_OK))
                self.assertTrue((package / ("install.ps1" if windows else "install.sh")).is_file())
                manifest = json.loads((package / "package.json").read_text())
                self.assertEqual(manifest["name"], f"@dcc/git-ai-{platform}")
                self.assertEqual(manifest["version"], self.version)
                self.assertEqual((package / ".npmrc").read_text(), "@dcc:registry=https://npm.intra.xiaojukeji.com/\n")

    def test_rejects_wrong_bundle_digest_and_incomplete_platform_set(self):
        result = self.stage(sha256="0" * 64)
        self.assertIn("bundle SHA-256 mismatch", result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())

        archives = self.archives.copy()
        archives.pop(f"dcc-git-ai-win32-x64-{self.version}.tgz")
        result = self.stage(archives=archives)
        self.assertIn("five platform tarballs", result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())

    def test_rejects_packages_from_another_commit(self):
        result = self.stage(source_commit="b" * 40)
        self.assertIn("source commit", result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())

    def test_rejects_tar_with_modified_payload(self):
        filename = f"dcc-git-ai-win32-x64-{self.version}.tgz"
        archive = self.archives[filename]
        for damage in ("symlink", "changed executable", "lifecycle script"):
            with self.subTest(damage=damage):
                modified = io.BytesIO()
                with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as original, tarfile.open(
                    fileobj=modified, mode="w:gz"
                ) as replacement:
                    for member in original.getmembers():
                        data = original.extractfile(member).read()
                        if member.name == "package/bin/dcc-ai-tracking.exe":
                            if damage == "symlink":
                                member.type = tarfile.SYMTYPE
                                member.linkname = "../../unexpected.exe"
                                data = b""
                            if damage == "changed executable":
                                data = b"different executable"
                        if member.name == "package/package.json" and damage == "lifecycle script":
                            manifest = json.loads(data)
                            manifest["scripts"] = {"prepublishOnly": "exit 1"}
                            data = json.dumps(manifest).encode()
                        member.size = len(data)
                        replacement.addfile(member, io.BytesIO(data))
                result = self.stage(archives={**self.archives, filename: modified.getvalue()})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("win32-x64", result.stderr)
                self.assertFalse(self.output.exists())

    def test_preserves_existing_output_directory(self):
        self.output.mkdir()
        sentinel = self.output / "existing.txt"
        sentinel.write_text("existing output")
        result = self.stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("output already exists", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_text(), "existing output")


if __name__ == "__main__":
    unittest.main()
