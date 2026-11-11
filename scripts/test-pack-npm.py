import hashlib
import json
import os
import platform as host_platform
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("pack-npm.py")


class NpmPackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="git-ai-npm-")
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "git-ai"\nversion = "1.7.7-dcc.1"\n')
        (self.root / "LICENSE").write_text("Apache license fixture\n")
        for installer in ("install.sh", "install.ps1"):
            (self.root / installer).write_bytes((SCRIPT.parent.parent / installer).read_bytes())
        self.binary = self.root / "dcc-ai-tracking"
        self.binary.write_bytes(b"signed native binary fixture\x00")
        self.metadata = self.root / "cargo-metadata.json"
        self.metadata.write_text(json.dumps({"packages": [
            {"name": "git-ai", "version": "1.7.7-dcc.1", "source": None},
            {"name": "example", "version": "2.0.0", "license": "MIT", "repository": "https://example.test/src"},
        ]}))

    def tearDown(self):
        self.temporary.cleanup()

    def pack(self, platform="darwin-arm64", source="a" * 40):
        return subprocess.run([
            sys.executable, str(SCRIPT), "--root", str(self.root), "--binary", str(self.binary),
            "--platform", platform, "--output", str(self.root / "out"), "--source-commit", source,
            "--cargo-metadata", str(self.metadata),
        ], text=True, capture_output=True, timeout=10)

    def test_pack_contains_binary_installer_metadata_and_licenses(self):
        result = self.pack()
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.root / "out" / "dcc-git-ai-darwin-arm64-1.7.7-dcc.1.tgz"
        with tarfile.open(archive) as package:
            self.assertEqual(set(package.getnames()), {
                "package/package.json", "package/bin/dcc-ai-tracking", "package/LICENSE", "package/THIRD-PARTY-NOTICES", "package/install.sh",
            })
            info = json.load(package.extractfile("package/package.json"))
            self.assertEqual(info["name"], "@dcc/git-ai-darwin-arm64")
            self.assertEqual(info["version"], "1.7.7-dcc.1")
            self.assertEqual(info["os"], ["darwin"])
            self.assertEqual(info["cpu"], ["arm64"])
            self.assertEqual(info["dcc"], {
                "sourceCommit": "a" * 40,
                "binarySha256": hashlib.sha256(self.binary.read_bytes()).hexdigest(),
            })
            for unwanted in ("scripts", "dependencies", "optionalDependencies", "main", "bin"):
                self.assertNotIn(unwanted, info)
            self.assertEqual(package.extractfile("package/bin/dcc-ai-tracking").read(), self.binary.read_bytes())
            self.assertEqual(package.getmember("package/bin/dcc-ai-tracking").mode, 0o755)
            self.assertIn("example 2.0.0 — MIT", package.extractfile("package/THIRD-PARTY-NOTICES").read().decode())

    def test_platform_packages_have_exact_platform_constraints(self):
        for platform, os_name, cpu, libc in [
            ("darwin-arm64", "darwin", "arm64", None),
            ("darwin-x64", "darwin", "x64", None),
            ("linux-arm64-musl", "linux", "arm64", ["musl"]),
            ("linux-x64-musl", "linux", "x64", ["musl"]),
            ("win32-x64", "win32", "x64", None),
        ]:
            with self.subTest(platform=platform):
                result = self.pack(platform)
                self.assertEqual(result.returncode, 0, result.stderr)
                archive = self.root / "out" / f"dcc-git-ai-{platform}-1.7.7-dcc.1.tgz"
                with tarfile.open(archive) as package:
                    info = json.load(package.extractfile("package/package.json"))
                    self.assertEqual(info["name"], f"@dcc/git-ai-{platform}")
                    self.assertEqual((info["os"], info["cpu"], info.get("libc")), ([os_name], [cpu], libc))
                    binary = "dcc-ai-tracking.exe" if os_name == "win32" else "dcc-ai-tracking"
                    installer = "install.ps1" if os_name == "win32" else "install.sh"
                    self.assertEqual(info["files"], [f"bin/{binary}", "LICENSE", "THIRD-PARTY-NOTICES", installer])
                    self.assertTrue(package.getmember(f"package/{installer}").isfile())
                    self.assertEqual(package.extractfile(f"package/bin/{binary}").read(), self.binary.read_bytes())
                    self.assertEqual(package.getmember(f"package/bin/{binary}").mode, 0o755)

    @unittest.skipUnless(host_platform.system() in ("Darwin", "Linux"), "requires Bash")
    def test_npm_upgrade_replaces_binary_without_running_first_install(self):
        home = self.root / "home"
        target = home / ".dcc-ai-tracking/bin/dcc-ai-tracking"
        target.parent.mkdir(parents=True)
        target.write_bytes(b"previous executable")
        self.binary.write_bytes(b'#!/bin/sh\n: > "$HOME/installer-called-binary"\n')
        result = subprocess.run(
            ["/bin/bash", str(self.root / "install.sh")],
            env={"HOME": str(home), "PATH": "/usr/bin:/bin", "SHELL": "/bin/bash",
                 "GIT_AI_NPM_UPGRADE": "1", "GIT_AI_LOCAL_BINARY": str(self.binary),
                 "GIT_AI_ALLOW_SUPERUSER": "1"},
            stdin=subprocess.DEVNULL, capture_output=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(target.read_bytes(), self.binary.read_bytes())
        self.assertTrue(os.access(target, os.X_OK))
        self.assertFalse((home / "installer-called-binary").exists())
        self.assertFalse((home / ".local/bin/dcc-ai-tracking").exists())

    def test_existing_version_archive_is_not_overwritten(self):
        self.assertEqual(self.pack().returncode, 0)
        archive = self.root / "out" / "dcc-git-ai-darwin-arm64-1.7.7-dcc.1.tgz"
        original = archive.read_bytes()
        self.binary.write_bytes(b"different binary")
        self.assertNotEqual(self.pack().returncode, 0)
        self.assertEqual(archive.read_bytes(), original)

    def test_unverified_source_and_unsupported_platforms_are_rejected(self):
        self.assertNotEqual(self.pack(source="short-sha").returncode, 0)
        self.assertNotEqual(self.pack(platform="win32-arm64").returncode, 0)
        self.assertFalse((self.root / "out").exists())

    def test_mismatched_cargo_metadata_is_rejected_before_packing(self):
        self.metadata.write_text(json.dumps({"packages": [{"name": "git-ai", "version": "1.7.6"}]}))
        result = self.pack()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("metadata does not match", result.stderr)
        self.assertFalse((self.root / "out").exists())

    def test_missing_license_or_symlink_binary_is_rejected(self):
        (self.root / "LICENSE").unlink()
        self.assertNotEqual(self.pack().returncode, 0)
        self.binary.unlink()
        self.binary.symlink_to(self.metadata)
        self.assertIn("regular file", self.pack().stderr)


if __name__ == "__main__":
    unittest.main()
