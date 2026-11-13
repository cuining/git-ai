import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "build-oe-npm.py"
VERSION = re.search(r'^version = "([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.MULTILINE)[1]

CARGO = r'''
import json, os, pathlib, sys
args = sys.argv[1:]
assert args[0] in ("metadata", "zigbuild", "xwin")
assert "--offline" in args and "--locked" in args
assert pathlib.Path(args[args.index("--config") + 1]).is_file()
version = os.environ["FIXTURE_VERSION"]
if args[0] == "metadata":
    print(json.dumps({"packages": [{"name": "git-ai", "version": version}]}))
    sys.exit(0)
assert os.environ["OSS_BUILD"] == "0"
assert os.environ["CARGO_INCREMENTAL"] == "0"
assert "SENTRY_OSS" not in os.environ and "POSTHOG_API_KEY" not in os.environ
target = args[args.index("--target") + 1]
assert ("SDKROOT" in os.environ) == target.endswith("apple-darwin")
assert (args[:2] == ["xwin", "build"]) == target.endswith("windows-msvc")
if target == os.environ.get("FAIL_TARGET"):
    sys.exit(37)
binary = pathlib.Path(args[args.index("--target-dir") + 1]) / target / "release" / "git-ai"
if target.endswith("windows-msvc"):
    assert "target-feature=+crt-static" in os.environ["RUSTFLAGS"]
    binary = binary.with_suffix(".exe")
binary.parent.mkdir(parents=True)
binary.write_text(f"#!/bin/sh\nprintf '%s\\n' '{version}'\n# target: {target}\n")
binary.chmod(0o755)
'''

READOBJ = r'''
import os, pathlib, sys
target = pathlib.Path(sys.argv[-1]).parent.parent.name
formats = {
    "aarch64-apple-darwin": ("Mach-O arm64", "aarch64"),
    "x86_64-apple-darwin": ("Mach-O 64-bit x86-64", "x86_64"),
    "aarch64-unknown-linux-musl": ("elf64-littleaarch64", "aarch64"),
    "x86_64-unknown-linux-musl": ("elf64-x86-64", "x86_64"),
    "x86_64-pc-windows-msvc": ("COFF-x86-64", "x86_64"),
}
kind, arch = formats[target]
if target == os.environ.get("WRONG_ARCH"):
    arch = "i386"
print(f"Format: {kind}\nArch: {arch}")
'''

CODESIGN = r'''
import os, pathlib, sys
binary = pathlib.Path(sys.argv[-1])
if sys.argv[1] == "sign":
    with binary.open("ab") as out:
        out.write(b"# signed fixture\n")
elif sys.argv[1] == "verify":
    assert binary.read_bytes().endswith(b"# signed fixture\n")
    print("CMS error: missing further values (at position 0)")
    if os.environ.get("BAD_SIGNATURE"):
        print("code digest mismatch at index 0")
    print("Error: problems reported during verification", file=sys.stderr)
    sys.exit(1)
else:
    sys.exit(1)
'''


class OeNpmBuildTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="git-ai-oe-build-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.output = self.root / "output"
        config = self.root / "config.toml"
        config.touch()
        for name, code in (("cargo", CARGO), ("llvm-readobj", READOBJ), ("rcodesign", CODESIGN)):
            command = self.root / name
            command.write_text(f"#!{sys.executable}\n" + code)
            command.chmod(0o755)
        self.command = [
            sys.executable, str(SCRIPT), "--work-dir", str(self.root / "work"),
            "--output", str(self.output), "--", str(self.root / "cargo"),
            "--offline", "--config", str(config),
        ]

    def build(self, **overrides):
        return subprocess.run(self.command, cwd=ROOT, env={
            **os.environ, "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
            "FIXTURE_VERSION": VERSION, "SDKROOT": str(self.root / "darwin-sysroot"),
            "SENTRY_OSS": "fixture", "POSTHOG_API_KEY": "fixture", **overrides,
        }, text=True, capture_output=True, timeout=20)

    def test_build_stages_verified_platforms_with_exact_source_and_final_signed_bytes(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list((self.root / "work").rglob("*.tgz")), [])
        self.assertFalse((self.root / "work" / "platform-packages.zip").exists())
        source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        platforms = {"darwin-arm64", "darwin-x64", "linux-arm64-musl", "linux-x64-musl", "win32-x64"}
        self.assertEqual({path.name for path in self.output.iterdir()}, platforms)
        for platform in platforms:
            with self.subTest(platform=platform):
                package = self.output / platform
                manifest = json.loads((package / "package.json").read_text())
                executable = "git-ai.exe" if platform == "win32-x64" else "git-ai"
                content = (package / "bin" / executable).read_bytes()
                self.assertEqual(manifest["name"], f"@dcc/git-ai-{platform}")
                self.assertEqual(manifest["version"], VERSION)
                self.assertEqual(manifest["dcc"], {
                    "sourceCommit": source, "binarySha256": hashlib.sha256(content).hexdigest(),
                })
                self.assertEqual(content.endswith(b"# signed fixture\n"), platform.startswith("darwin-"))

    def test_build_configuration_is_validated_regardless_of_publish_tag(self):
        for tag in (None, "dcc", "test-candidate"):
            with self.subTest(tag=tag):
                env = {**os.environ, "DCC_OE_BUILD_NPM": "invalid"}
                env.pop("PUBLISH_DIST_TAG", None)
                if tag is not None:
                    env["PUBLISH_DIST_TAG"] = tag
                result = subprocess.run(
                    ["sh", str(ROOT / "build.sh")], cwd=ROOT, env=env,
                    text=True, capture_output=True, timeout=5,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stderr.strip(), "DCC_OE_BUILD_NPM must be 0 or 1")

    def test_oe_requires_bootstrap_before_running_tools(self):
        env = {**os.environ, "PATH": f"{self.root}{os.pathsep}/usr/bin:/bin",
               "CARGO_HOME": str(self.root), "DCC_OE_BUILD_NPM": "1"}
        env.pop("DCC_OE_BOOTSTRAP_URL", None)
        env.pop("DCC_OE_BOOTSTRAP_SHA256", None)
        for name in ("curl", "rustup", "timeout"):
            tool = self.root / name
            tool.write_text(f"#!/bin/sh\necho unexpected {name} >&2\nexit 1\n")
            tool.chmod(0o755)
        result = subprocess.run(
            ["sh", str(ROOT / "build.sh")], cwd=ROOT, env=env,
            text=True, capture_output=True, timeout=5,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stderr.strip(), "OE bootstrap requires both URL and SHA-256")

    def test_failed_windows_build_cannot_expose_partial_output(self):
        result = self.build(FAIL_TARGET="x86_64-pc-windows-msvc")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("returned non-zero exit status 37", result.stderr)
        self.assertFalse(self.output.exists())

    def test_rejects_unexpected_target_architecture(self):
        result = self.build(WRONG_ARCH="aarch64-apple-darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("darwin-arm64", result.stderr)
        self.assertFalse(self.output.exists())

    def test_rejects_signature_digest_failure_even_with_expected_empty_cms(self):
        result = self.build(BAD_SIGNATURE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("code digest mismatch", result.stderr)
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
