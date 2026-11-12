"""Verify a five-platform npm bundle and stage it for the OE npm node."""

import argparse
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import stat
import tarfile
import tempfile
import zipfile


PACKER_SPEC = importlib.util.spec_from_file_location("pack_npm", Path(__file__).with_name("pack-npm.py"))
PACKER = importlib.util.module_from_spec(PACKER_SPEC)
PACKER_SPEC.loader.exec_module(PACKER)
NPM_REGISTRY = "https://npm.intra.xiaojukeji.com/"


def read_package(data, platform, version, source_commit):
    os_name, cpu = PACKER.PLATFORMS[platform]
    binary = "bin/git-ai.exe" if os_name == "win32" else "bin/git-ai"
    installer = "install.ps1" if os_name == "win32" else "install.sh"
    files = [binary, "LICENSE", "THIRD-PARTY-NOTICES", installer]
    expected = {f"package/{name}" for name in ["package.json", *files]}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as package:
        members = package.getmembers()
        if len(members) != len(expected) or {member.name for member in members} != expected:
            raise ValueError(f"unexpected archive entries for {platform}")
        payload = {}
        for member in members:
            name = member.name.split("/", 1)[1]
            mode = 0o755 if name == binary else 0o644
            if member.type != tarfile.REGTYPE or member.mode != mode:
                raise ValueError(f"unexpected file type or mode for {platform}/{name}")
            payload[name] = (package.extractfile(member).read(), mode)

    executable = payload[binary][0]
    manifest = {
        "name": f"@dcc/git-ai-{platform}",
        "version": version,
        "license": "Apache-2.0",
        "os": [os_name],
        "cpu": [cpu],
        "files": files,
        "dcc": {"sourceCommit": source_commit, "binarySha256": hashlib.sha256(executable).hexdigest()},
    }
    if os_name == "linux":
        manifest["libc"] = ["musl"]
    if not executable or json.loads(payload["package.json"][0]) != manifest:
        raise ValueError(f"manifest, source commit, or executable checksum mismatch for {platform}")
    return payload


def stage_packages(archive_names, read_archive, source_commit, output):
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        raise ValueError("source commit must be a full lowercase Git SHA")
    if output.exists() or output.is_symlink():
        raise ValueError(f"output already exists: {output}")
    version = PACKER.cargo_version(Path(__file__).resolve().parents[1])
    archives = {f"dcc-git-ai-{platform}-{version}.tgz": platform for platform in PACKER.PLATFORMS}
    if len(archive_names) != len(archives) or set(archive_names) != archives.keys():
        raise ValueError("bundle must contain the five platform tarballs for this version")

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as temporary:
        staged = Path(temporary) / "packages"
        for archive, platform in archives.items():
            package = read_package(read_archive(archive), platform, version, source_commit)
            for name, (data, mode) in package.items():
                path = staged / platform / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                path.chmod(mode)
            (staged / platform / ".npmrc").write_text(f"@dcc:registry={NPM_REGISTRY}\n")
        staged.rename(output)


def stage(bundle, sha256, source_commit, output):
    with bundle.open("rb") as source:
        digest = hashlib.sha256()
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
        if digest.hexdigest() != sha256:
            raise ValueError("bundle SHA-256 mismatch")
        with zipfile.ZipFile(source) as zipped:
            for entry in zipped.infolist():
                if stat.S_IFMT(entry.external_attr >> 16) not in (0, stat.S_IFREG):
                    raise ValueError(f"unexpected ZIP file type: {entry.filename}")
            stage_packages(zipped.namelist(), zipped.read, source_commit, output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        stage(args.bundle, args.sha256, args.source_commit, args.output)
    except (EOFError, OSError, RuntimeError, ValueError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.exit(1, f"OE npm staging failed: {error}\n")


if __name__ == "__main__":
    main()
