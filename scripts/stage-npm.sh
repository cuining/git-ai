#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 3 ]]; then
    echo "Usage: $0 <npm-package-prefix> [release-dir] [output-dir]" >&2
    exit 1
fi

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/cargo-package-version.sh
source "$root/scripts/cargo-package-version.sh"
package_prefix="$1"
release_dir="${2:-$root/output}"
output_dir="${3:-$root/output/npm}"
package_pattern='^(@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$'

if [[ ! "$package_prefix" =~ $package_pattern ]]; then
    echo "Invalid npm package prefix: $package_prefix" >&2
    exit 1
fi

version="$(cargo_package_version "$root/Cargo.toml")"
binary_name="$(cargo_package_field "$root/Cargo.toml" default-run)"
packages=()
while read -r package; do
    read -r artifact _ <<< "$package"
    binary="$release_dir/$artifact"
    if [[ ! -f "$binary" ]]; then
        continue
    fi
    if [[ ! -s "$binary" ]]; then
        echo "Empty release binary: $binary" >&2
        exit 1
    fi
    if [[ ! -f "$binary.version" || "$(<"$binary.version")" != "$version" ]]; then
        echo "Expected a version $version build of $binary with its .version file" >&2
        exit 1
    fi
    packages+=("$package")
done <<ARTIFACTS
$binary_name-linux-x64 linux x64 $binary_name
$binary_name-linux-arm64 linux arm64 $binary_name
$binary_name-macos-x64 darwin x64 $binary_name
$binary_name-macos-arm64 darwin arm64 $binary_name
$binary_name-windows-x64.exe win32 x64 $binary_name.exe
$binary_name-windows-arm64.exe win32 arm64 $binary_name.exe
ARTIFACTS

if [[ "${#packages[@]}" -eq 0 ]]; then
    echo "No platform binaries found in $release_dir" >&2
    exit 1
fi

mkdir -p -- "$output_dir"
existing_entry="$(find -H "$output_dir" -mindepth 1 -maxdepth 1 -print -quit)"
if [[ -n "$existing_entry" ]]; then
    echo "Use an empty npm output directory: $output_dir" >&2
    exit 1
fi

for package in "${packages[@]}"; do
    read -r artifact os cpu executable <<< "$package"
    platform="$os-$cpu"
    package_dir="$output_dir/$platform"
    binary_path="bin/$executable"
    mkdir -p "$package_dir/${binary_path%/*}"
    install -m 755 "$release_dir/$artifact" "$package_dir/$binary_path"
    cp "$root/LICENSE" "$package_dir/LICENSE"
    cat > "$package_dir/package.json" <<MANIFEST
{
  "name": "$package_prefix-$platform",
  "version": "$version",
  "license": "Apache-2.0",
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["$binary_path"]
}
MANIFEST
    printf '%s\n' "$package_dir"
done
