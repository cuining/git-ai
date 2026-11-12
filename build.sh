#!/bin/sh
set -eu

# OE runs this script from the checkout using the DCC builder image.
toolchain=1.93.0
build_npm=${DCC_OE_BUILD_NPM:-0}
release_sdk=${SDKROOT:-}
case "$build_npm" in
    0|1) ;;
    *) printf '%s\n' 'DCC_OE_BUILD_NPM must be 0 or 1' >&2; exit 1 ;;
esac
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

if [ -n "${DCC_OE_BOOTSTRAP_URL:-}${DCC_OE_BOOTSTRAP_SHA256:-}" ]; then
    if [ -z "${DCC_OE_BOOTSTRAP_URL:-}" ] || [ -z "${DCC_OE_BOOTSTRAP_SHA256:-}" ]; then
        printf '%s\n' 'OE bootstrap requires both URL and SHA-256' >&2
        exit 1
    fi
    archive=$scratch/bootstrap.tar.gz
    bootstrap=$scratch/bootstrap
    curl --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 600 \
        -fsSL --output "$archive" -- "$DCC_OE_BOOTSTRAP_URL"
    digest=$(sha256sum "$archive")
    if [ "${digest%% *}" != "$DCC_OE_BOOTSTRAP_SHA256" ]; then
        printf '%s\n' 'OE bootstrap SHA-256 mismatch' >&2
        exit 1
    fi
    mkdir "$bootstrap"
    # The release bootstrap joins separately verified test and cross-tool tar members.
    tar -xizf "$archive" -C "$bootstrap"
    rm "$archive"
    if [ -f "$bootstrap/toolchain/release-tools-manifest.json" ]; then
        build_npm=${DCC_OE_BUILD_NPM:-1}
        release_sdk=$bootstrap/toolchain/darwin-sysroot
    fi
    for binary in cargo rustc; do
        version=$("$bootstrap/toolchain/bin/$binary" --version)
        case "$version" in
            "$binary $toolchain "*) ;;
            *) printf 'Expected %s %s, got %s\n' "$binary" "$toolchain" "$version" >&2; exit 1 ;;
        esac
    done
    export PATH="$bootstrap/toolchain/bin:$PATH"
    export RUSTC="$bootstrap/toolchain/bin/rustc"
    set -- "$bootstrap/toolchain/bin/cargo"
    if [ -d "$bootstrap/vendor" ] || [ -f "$bootstrap/cargo-vendor.toml" ]; then
        if [ ! -d "$bootstrap/vendor" ] || [ ! -f "$bootstrap/cargo-vendor.toml" ]; then
            printf '%s\n' 'OE bootstrap must include both vendor/ and cargo-vendor.toml' >&2
            exit 1
        fi
        mkdir "$bootstrap/.cargo"
        cp "$bootstrap/cargo-vendor.toml" "$bootstrap/.cargo/config.toml"
        export CARGO_HOME="$scratch/cargo-home"
        set -- "$@" --offline --config "$bootstrap/.cargo/config.toml"
    fi
else
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    if ! command -v rustup >/dev/null 2>&1; then
        installer=$scratch/rustup-init.sh
        curl --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 60 \
            -fsSL https://sh.rustup.rs -o "$installer"
        timeout -k 5 180 sh "$installer" -y --no-modify-path --profile minimal --default-toolchain none
    fi
    if ! timeout -k 5 20 rustup run "$toolchain" cargo --version; then
        timeout -k 5 180 rustup toolchain install "$toolchain" --profile minimal
    fi
    set -- rustup run "$toolchain" cargo
fi

if [ "$build_npm" = 1 ] && [ -n "${DCC_NPM_BUNDLE_URL:-}${DCC_NPM_BUNDLE_SHA256:-}" ]; then
    printf '%s\n' 'Choose either an OE source build or a prebuilt npm bundle' >&2
    exit 1
fi
python3 scripts/test-pack-npm.py
python3 scripts/test-stage-oe-npm.py
python3 scripts/test-build-oe-npm.py
git --version
# Match the library-only Taskfile test invocation with a bounded runner pool.
GIT_AI_TEST_SHARED_DAEMON_POOL_SIZE=4 "$@" test --locked --lib -- --test-threads 4
# Compile integration tests without the bootstrap's library-only Cargo runner.
"$@" build --locked --bin git-ai --features test-support --target-dir target
"$@" test --locked --test integration --target-dir target --no-run --message-format=json \
    > "$scratch/usage-tests.jsonl"
usage_test=$(python3 - "$scratch/usage-tests.jsonl" <<'PY'
import json
import sys

with open(sys.argv[1]) as events:
    tests = [event["executable"] for event in map(json.loads, events)
             if event.get("reason") == "compiler-artifact"
             and event["target"]["name"] == "integration"
             and event.get("executable")]
if len(tests) != 1:
    raise ValueError("Expected one compiled integration test executable")
print(tests[0])
PY
)
GIT_AI_TEST_BINARY_PATH="$PWD/target/debug/git-ai" GIT_AI_TEST_SHARED_DAEMON_POOL_SIZE=4 \
    "$usage_test" usage_period::usage_machine_ --test-threads 4

if [ "$build_npm" = 1 ]; then
    SDKROOT="$release_sdk" python3 scripts/build-oe-npm.py --work-dir "$scratch/release" --output output -- "$@"
elif [ -n "${DCC_NPM_BUNDLE_URL:-}${DCC_NPM_BUNDLE_SHA256:-}" ]; then
    bundle=$scratch/platform-packages.zip
    curl --proto '=https' --proto-redir '=https' -fsSL --retry 2 --max-time 300 \
        "${DCC_NPM_BUNDLE_URL:?missing npm bundle URL}" -o "$bundle"
    python3 scripts/stage-oe-npm.py --bundle "$bundle" \
        --sha256 "${DCC_NPM_BUNDLE_SHA256:?missing npm bundle SHA-256}" \
        --source-commit "$(git rev-parse HEAD)" --output output
fi

mkdir -p output
printf '%s\n' 'Rust library and npm packaging tests passed' > output/test-result.txt
