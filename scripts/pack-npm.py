"""Pack one already-built and signed dcc-ai-tracking platform binary with its upgrade installer."""

import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile


PLATFORMS = {
    "darwin-arm64": ("darwin", "arm64"),
    "darwin-x64": ("darwin", "x64"),
    "linux-arm64-musl": ("linux", "arm64"),
    "linux-x64-musl": ("linux", "x64"),
    "win32-x64": ("win32", "x64"),
}


def cargo_version(root):
    cargo = (root / "Cargo.toml").read_text()
    section = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", cargo)
    version = re.search(r'^version\s*=\s*"([^"]+)"', section[1] if section else "", re.MULTILINE)
    if not version or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version[1]):
        raise ValueError("Cargo.toml must declare an exact package version")
    return version[1]


def notices(metadata, version):
    packages = json.loads(metadata.read_text())["packages"]
    roots = [p for p in packages if p["name"] == "git-ai" and not p.get("source")]
    if len(roots) != 1 or roots[0]["version"] != version:
        raise ValueError("Cargo metadata does not match this git-ai version")
    lines = ["Third-party components resolved by Cargo", "", "See each project's license and source for its terms.", ""]
    for package in sorted(packages, key=lambda p: (p["name"], p["version"])):
        if package is roots[0]:
            continue
        lines.append(f'{package["name"]} {package["version"]} — {package.get("license") or "license not declared"}')
        if package.get("repository") or package.get("source"):
            lines.append(package.get("repository") or package["source"])
    return ("\n".join(lines) + "\n").encode()


def pack(root, binary, platform, output, source_commit, cargo_metadata):
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        raise ValueError("source commit must be a full lowercase Git SHA")
    if binary.is_symlink() or not binary.is_file():
        raise ValueError("binary must be a regular file")
    content = binary.read_bytes()
    if not content:
        raise ValueError("binary is empty")
    version = cargo_version(root)
    os_name, cpu = PLATFORMS[platform]
    binary_path = "bin/dcc-ai-tracking.exe" if os_name == "win32" else "bin/dcc-ai-tracking"
    installer = "install.ps1" if os_name == "win32" else "install.sh"
    digest = hashlib.sha256(content).hexdigest()
    asset = Path(binary_path).name
    prefix = b"$EmbeddedChecksums = '" if os_name == "win32" else b'EMBEDDED_CHECKSUMS="'
    template = (root / installer).read_bytes()
    placeholder = prefix + b"__CHECKSUMS_PLACEHOLDER__"
    if template.count(placeholder) != 1:
        raise ValueError(f"missing checksum placeholder in {installer}")
    script = template.replace(placeholder, prefix + f"{digest}  {asset}".encode(), 1)
    info = {
        "name": f"@dcc/git-ai-{platform}",
        "version": version,
        "license": "Apache-2.0",
        "os": [os_name],
        "cpu": [cpu],
        "files": [binary_path, "LICENSE", "THIRD-PARTY-NOTICES", installer],
        "dcc": {"sourceCommit": source_commit, "binarySha256": digest},
    }
    if os_name == "linux":
        info["libc"] = ["musl"]
    files = {
        "package.json": (json.dumps(info, indent=2) + "\n").encode(),
        binary_path: content,
        installer: script,
        "LICENSE": (root / "LICENSE").read_bytes(),
        "THIRD-PARTY-NOTICES": notices(cargo_metadata, version),
    }
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"dcc-git-ai-{platform}-{version}.tgz"
    with tarfile.open(archive, "x:gz", format=tarfile.USTAR_FORMAT) as package:
        for name, data in files.items():
            entry = tarfile.TarInfo(f"package/{name}")
            entry.size = len(data)
            entry.mode = 0o755 if name == binary_path else 0o644
            package.addfile(entry, io.BytesIO(data))
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--cargo-metadata", type=Path, required=True)
    args = parser.parse_args()
    try:
        print(pack(args.root, args.binary, args.platform, args.output, args.source_commit, args.cargo_metadata))
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"npm packaging failed: {error}\n")


if __name__ == "__main__":
    main()
