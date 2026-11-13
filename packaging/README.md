# Git AI packaging

This directory contains MSI and PKG installer scaffolding for Git AI.

## DCC npm platform packages

DCC builds in OE and publishes to the internal npm registry. The five packages
are built with `OSS_BUILD=0` and without baked OSS telemetry keys:
`@dcc/git-ai-darwin-{arm64,x64}`, `@dcc/git-ai-linux-{arm64,x64}-musl`, and
`@dcc/git-ai-win32-x64`.

Each package contains `package.json`, `LICENSE`, `THIRD-PARTY-NOTICES`, and
the executable at `bin/git-ai` on macOS and Linux or
`bin/git-ai.exe` on Windows. The package also contains `install.sh`
or `install.ps1` for explicit use by the standalone updater. The packages
have no dependencies or npm lifecycle scripts.
The notices list the resolved Cargo dependencies, SPDX licenses and sources.
Review dependency license obligations before distribution; this inventory
does not replace upstream license texts where those are required.

`scripts/pack-npm.py` uses Python's standard library, hashes the final signed
binary, and records `dcc.sourceCommit` and `dcc.binarySha256`. It reads the
package version from Cargo.toml and verifies matching Cargo metadata.

Enterprise installations use the `latest` tag on the internal npm registry for
`git-ai upgrade` and daemon updates. Daemon updates preserve initialized hooks
and configuration. Run `git-ai install` from the verified bundled executable
to install it and set up hooks. A completed installation is reused. Run
`git-ai install-hooks` from the installed executable to refresh hooks.

Setup checks Claude Code, Codex, Cursor, OpenCode, Pi, and DeepSeek Harness
Desktop (macOS and Windows). Cursor gets hooks only, and Codex defaults to
`config.toml`. On Windows, the Visual Studio extension requires
`--visual-studio-extension`.

DCC downloads and extracts these packages without an installer confirmation
step. This does not guarantee that endpoint security or macOS policy will never
prompt or block execution. The distribution flow does not remove quarantine
attributes or disable Gatekeeper.

Local packaging tests do not compile Rust, install hooks or access a registry:

```bash
python3 scripts/test-pack-npm.py
```

### Publish from OE master

Use an OE Docker compile task with the DCC builder `:e2e` image to run
`build.sh`. The script requires `DCC_OE_BOOTSTRAP_URL` and
`DCC_OE_BOOTSTRAP_SHA256` for the verified toolchain archive. The release
archive supplies Rust, vendored Cargo
dependencies, the cross compilers and a `toolchain/release-tools-manifest.json`
marker. The Windows MSVC build also needs access to the pinned Windows SDK and
CRT through cargo-xwin.

The compile task runs the Rust library tests, packaging tests, and dated usage
integration tests (`usage_period::usage_machine_`) before building the five
binaries from the same checkout. It checks their format, architecture and
embedded version, runs `--version` on Linux x64, and ad hoc signs the macOS
binaries. It stages the five verified tarballs directly as publish directories
under `output/`. A failed test or build prevents package staging.

Run the compile task from `master` and attach its output to a downstream
**NPM包发布** task. Set
`BATCH_PUBLISH_DIR=darwin-arm64,darwin-x64,linux-arm64-musl,linux-x64-musl,win32-x64`.
Set the publish task's default `PUBLISH_DIST_TAG` to `latest` once. npm requires
an explicit tag for prerelease versions such as `1.7.7-dcc.12`. For each release,
bump the version in `Cargo.toml` and `Cargo.lock`. Before DCC adopts it, check
that all five registry entries report the expected version and source commit.

For prebuilt releases, use the test toolchain archive and supply
`DCC_NPM_BUNDLE_URL` and `DCC_NPM_BUNDLE_SHA256`. The ZIP must contain the five
flat `pack-npm.py` tarballs from the exact OE checkout and its Cargo version.

## MSI and PKG installers

Package outputs must install `git-ai` only. They must not install a `git`
wrapper, `git.exe` shim, `git-og`, or any other executable that changes Git
command routing. Per-user trace2 and editor/agent setup remains the
responsibility of `git-ai install-hooks`, which both installers invoke with
`--env` to also apply the per-user shell/PATH configuration.

The release workflow builds signed/notarized production packages when the
required Apple and Azure signing secrets are configured. Dry-run releases can
build unsigned packages for validation. macOS releases include Intel, Apple
Silicon, and universal PKGs.

The Windows MSI is per-user: it installs under
`%USERPROFILE%\\.git-ai\\bin` and changes only that user's `PATH` (the MSI
owns the user PATH entry and removes it on uninstall; `install-hooks --env`
additionally configures Git Bash profiles when Git Bash is installed). It has
no all-users or Administrator install mode. For WSL, install the matching Linux
npm package inside the distribution.

The macOS PKG copies its bundled binary into the
active console user's `~/.git-ai/bin`, then runs setup as that user, including
adding `~/.git-ai/bin` to the user's shell PATH configs. It fails if no valid
console user is logged in or per-user setup fails.

For an enterprise endpoint, pass configuration to the MSI when installing:

```powershell
msiexec /i git-ai-windows-x64.msi API_BASE=https://usegitai.com API_KEY=your-api-key
```

These values configure only the installing user's Git AI config. They are
hidden from MSI logs, but command-line arguments can still be visible to local
process inspection and shell history. Use your endpoint-management secret
mechanism when available.
