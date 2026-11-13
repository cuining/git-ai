"""Cross-build and stage the five enterprise npm packages on a prepared OE runner."""

import argparse
import importlib.util
import os
from pathlib import Path
import re
import shutil
import subprocess


ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "darwin-arm64": ("aarch64-apple-darwin", "Mach-O arm64", "aarch64"),
    "darwin-x64": ("x86_64-apple-darwin", "Mach-O 64-bit x86-64", "x86_64"),
    "linux-arm64-musl": ("aarch64-unknown-linux-musl", "elf64-littleaarch64", "aarch64"),
    "linux-x64-musl": ("x86_64-unknown-linux-musl", "elf64-x86-64", "x86_64"),
    "win32-x64": ("x86_64-pc-windows-msvc", "COFF-x86-64", "x86_64"),
}


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cargo_command(cargo, subcommand, *args):
    executable, *options = cargo
    return [executable, *subcommand, *options, *args]


def sign_macos(binary):
    subprocess.run(["rcodesign", "sign", binary], check=True)
    result = subprocess.run(["rcodesign", "verify", binary], text=True, capture_output=True)
    if result.returncode == 0:
        return
    # rcodesign 0.29 verifies code digests but rejects its own empty ad-hoc CMS blob.
    if (result.returncode == 1 and result.stdout.strip() == "CMS error: missing further values (at position 0)"
            and "Error: problems reported during verification" in result.stderr):
        return
    raise ValueError(f"signature verification failed for {binary}: {result.stdout}{result.stderr}")


def verify_binary(binary, platform, version, work_dir):
    if binary.is_symlink() or not binary.is_file() or version.encode() not in binary.read_bytes():
        raise ValueError(f"missing or incorrect version in {platform} executable")
    _, file_format, architecture = TARGETS[platform]
    inspection = subprocess.check_output([
        "llvm-readobj", "--file-headers", "--program-headers", "--dynamic-table", "--coff-imports", binary,
    ], text=True)
    if f"Format: {file_format}" not in inspection.splitlines() or f"Arch: {architecture}" not in inspection.splitlines():
        raise ValueError(f"unexpected executable format or architecture for {platform}")
    if platform.startswith("linux-") and re.search(r"\b(?:PT_INTERP|NEEDED)\b", inspection):
        raise ValueError(f"expected a static musl executable for {platform}")
    if platform == "win32-x64" and re.search(r"Name:\s+(?:vcruntime|msvcp|ucrtbase|api-ms-win-crt)", inspection, re.I):
        raise ValueError("Windows executable imports a dynamic C runtime")
    if platform.startswith("darwin-"):
        sign_macos(binary)
    if platform == "linux-x64-musl":
        actual = subprocess.check_output([binary, "--version"], cwd=work_dir, text=True, timeout=30).strip()
        if actual != version:
            raise ValueError(f"unexpected version from {platform}: {actual}")


def build(cargo, work_dir, output):
    if output.exists() or output.is_symlink():
        raise ValueError(f"output already exists: {output}")
    for tool in ("llvm-readobj", "rcodesign"):
        if shutil.which(tool) is None:
            raise ValueError(f"required OE runner tool is missing: {tool}")
    packer = load_script("pack-npm")
    stager = load_script("stage-oe-npm")
    version = packer.cargo_version(ROOT)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    work_dir.mkdir(parents=True)
    target_dir = work_dir / "target"
    metadata = work_dir / "cargo-metadata.json"
    env = {**os.environ, "OSS_BUILD": "0", "CARGO_INCREMENTAL": "0"}
    env.pop("SENTRY_OSS", None)
    env.pop("POSTHOG_API_KEY", None)
    with metadata.open("wb") as file:
        subprocess.run(cargo_command(cargo, ["metadata"], "--locked", "--format-version", "1"),
                       cwd=ROOT, env=env, stdout=file, check=True)

    with stager.publish_directory(output) as staged:
        for platform in packer.PLATFORMS:
            target, _, _ = TARGETS[platform]
            target_env = env.copy()
            subcommand = ["zigbuild"]
            if platform.startswith("darwin-"):
                target_env["MACOSX_DEPLOYMENT_TARGET"] = "11.0" if platform == "darwin-arm64" else "10.15"
            else:
                target_env.pop("SDKROOT", None)
            if platform == "win32-x64":
                subcommand = ["xwin", "build"]
                target_env.pop("CARGO_ENCODED_RUSTFLAGS", None)
                target_env["RUSTFLAGS"] = "-C target-feature=+crt-static"
                target_env.setdefault("XWIN_SDK_VERSION", "10.0.26100")
                target_env.setdefault("XWIN_CRT_VERSION", "14.44.17.14")
            print(f"Building {platform} ({target})", flush=True)
            subprocess.run(cargo_command(cargo, subcommand, "--locked", "--release", "--bin", "git-ai",
                                         "--target", target, "--target-dir", target_dir, "--jobs", "2"),
                           cwd=ROOT, env=target_env, check=True)
            executable = "git-ai.exe" if platform == "win32-x64" else "git-ai"
            binary = target_dir / target / "release" / executable
            verify_binary(binary, platform, version, work_dir)
            package = packer.package_files(ROOT, binary, platform, version, source, metadata)
            stager.write_package(staged / platform, package)

        if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip() != source:
            raise ValueError("checkout changed during the release build")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("cargo", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not args.cargo or args.cargo[0] != "--" or len(args.cargo) == 1:
        parser.error("pass the Cargo invocation after --")
    try:
        build(args.cargo[1:], args.work_dir.resolve(), args.output)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"OE npm build failed: {error}\n")


if __name__ == "__main__":
    main()
