#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: ./build.sh <platform> [output-dir]

Platforms: linux-x64, linux-arm64, macos-x64, macos-arm64,
           windows-x64, windows-arm64

Builds one release binary and its .version file. The default output directory
is ./output in the repository. Run on a worker with the selected Rust target
and linker installed.
CARGO_TARGET_DIR can set the parent directory for per-target Cargo caches.
USAGE
}

if [[ "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
if [[ $# -lt 1 || $# -gt 2 ]]; then
    usage >&2
    exit 2
fi

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/cargo-package-version.sh
source "$root/scripts/cargo-package-version.sh"
platform="$1"
output_dir="${2:-$root/output}"
executable="git-ai"

case "$platform" in
    linux-x64) target="x86_64-unknown-linux-musl" ;;
    linux-arm64) target="aarch64-unknown-linux-musl" ;;
    macos-x64) target="x86_64-apple-darwin" ;;
    macos-arm64) target="aarch64-apple-darwin" ;;
    windows-x64) target="x86_64-pc-windows-msvc"; executable="git-ai.exe" ;;
    windows-arm64) target="aarch64-pc-windows-msvc"; executable="git-ai.exe" ;;
    *)
        echo "Unsupported platform: $platform" >&2
        usage >&2
        exit 2
        ;;
esac

version="$(cargo_package_version "$root/Cargo.toml")"
cache_dir="${CARGO_TARGET_DIR:-$root/target/platform-builds}"
mkdir -p -- "$cache_dir"
cache_dir="$(cd -- "$cache_dir" && pwd)/$target"

(
    cd -- "$root"
    cargo build --locked --release --bin git-ai --target "$target" --target-dir "$cache_dir"
)

binary="$cache_dir/$target/release/$executable"
artifact="$output_dir/git-ai-$platform${executable#git-ai}"
if [[ ! -f "$binary" || ! -s "$binary" ]]; then
    echo "Build did not produce a nonempty binary at $binary" >&2
    exit 1
fi

mkdir -p -- "$output_dir"
if [[ -d "$artifact" || -d "$artifact.version" ]]; then
    echo "Expected files at $artifact and $artifact.version" >&2
    exit 1
fi
temporary_artifact="$(mktemp "$artifact.XXXXXX")"
temporary_version="$temporary_artifact.version"
trap 'rm -f -- "$temporary_artifact" "$temporary_version"' EXIT
install -m 755 -- "$binary" "$temporary_artifact"
printf '%s\n' "$version" > "$temporary_version"
rm -f -- "$artifact.version"
mv -f -- "$temporary_artifact" "$artifact"
mv -f -- "$temporary_version" "$artifact.version"
printf 'Built %s\n' "$artifact"
