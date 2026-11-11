# Git AI packaging

This directory contains MSI and PKG installer scaffolding for Git AI.

## DCC npm platform packages

The Release Build workflow supports `enterprise_npm=true`. It builds Linux
musl, macOS, and Windows x64 with `OSS_BUILD=0` and without baked OSS telemetry
keys. It skips PKG and MSI packaging and GitHub release publication. The five
packages are
`@dcc/git-ai-darwin-{arm64,x64}`, `@dcc/git-ai-linux-{arm64,x64}-musl`, and
`@dcc/git-ai-win32-x64`.

Keep `release_production=false`. Start with `dry_run=true` and
`publish_npm=false` to inspect the `dcc-ai-tracking-npm-packages` artifact.
Each package contains `package.json`, `LICENSE`, `THIRD-PARTY-NOTICES`, and
the executable at `bin/dcc-ai-tracking` on macOS and Linux or
`bin/dcc-ai-tracking.exe` on Windows. The package also contains `install.sh`
or `install.ps1` for explicit use by the standalone updater. The packages
have no dependencies or npm lifecycle scripts.
The notices list the resolved Cargo dependencies, SPDX licenses and sources.
Review dependency license obligations before distribution; this inventory
does not replace upstream license texts where those are required.

`scripts/pack-npm.py` uses Python's standard library, hashes the final signed
binary, and records `dcc.sourceCommit` and `dcc.binarySha256`. It reads the
package version from Cargo.toml and verifies matching Cargo metadata. The
workflow executes `--version` on each native build host before allowing the
packaging job, so npm, Cargo and binary versions must match. Cross-platform
packaging itself never executes the input binary or modifies its signature.

To publish, configure the `npm-release` environment with required reviewers,
`DCC_NPM_REGISTRY` as an environment variable, and `DCC_NPM_TOKEN` as an
environment secret. Its runner must reach that registry. Explicitly dispatch
with `enterprise_npm=true`, `dry_run=false`, `publish_npm=true`; publication
uses only the tgz artifacts produced in that run, sets the `dcc` tag, and
verifies that every tagged package matches the uploaded bytes. Missing registry/token fails
instead of falling back to the public registry. Merely declaring an
environment in YAML does not configure its approval rules.

Do not overwrite an existing version. For an enterprise revision, set a new
Cargo version (for example `1.7.7-dcc.2`) before building, and bind DCC only
after all five packages have published and passed readback. A partial publish
is a failed release and must be resolved before DCC adopts it. In enterprise npm
mode, macOS artifacts use Developer ID signing when both signing secrets are
configured; otherwise they receive an ad hoc signature. Both paths must pass
`codesign --verify --strict` and the native version check before packaging.
OSS installer release signing requirements remain unchanged.

Starting with `1.7.7-dcc.4`, standalone enterprise installations use the
`dcc` tag on the internal npm registry for `dcc-ai-tracking upgrade`.
Daemon updates use the existing update settings and preserve initialized
hooks and configuration. DCC-managed installations keep DCC as their updater.

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
`build.sh`. Set `DCC_OE_BOOTSTRAP_URL` and `DCC_OE_BOOTSTRAP_SHA256` to the
verified release toolchain archive. It supplies Rust, vendored Cargo
dependencies, the cross compilers and a `toolchain/release-tools-manifest.json`
marker. The Windows MSVC build also needs access to the pinned Windows SDK and
CRT through cargo-xwin.

The compile task runs the Rust library tests, packaging tests, and dated usage
integration tests (`usage_period::usage_machine_`) before building the five
binaries from the same checkout. It checks the target format, architecture and
version, signs the macOS binaries, then stages the verified packages under
`output/`. A failed test or build prevents package staging.

Run the compile task from `master` and attach its output to a downstream
**NPM包发布** task. Set
`BATCH_PUBLISH_DIR=darwin-arm64,darwin-x64,linux-arm64-musl,linux-x64-musl,win32-x64`.
Set the publish task's default `PUBLISH_DIST_TAG` to `latest` once. npm requires
an explicit tag for prerelease versions such as `1.7.7-dcc.12`. For each release,
bump the version in `Cargo.toml` and `Cargo.lock`. Check that all five registry
entries report the expected version and source commit.

For prebuilt releases, use the test toolchain archive and supply
`DCC_NPM_BUNDLE_URL` and `DCC_NPM_BUNDLE_SHA256`. The ZIP must contain the five
flat `pack-npm.py` tarballs from the exact OE checkout and its Cargo version.

## MSI and PKG installers

Package outputs must install `dcc-ai-tracking` only. They must not install a `git`
wrapper, `git.exe` shim, `git-og`, or any other executable that changes Git
command routing. Per-user trace2 and editor/agent setup remains the
responsibility of `dcc-ai-tracking install-hooks`, which both installers invoke with
`--env` to also apply the per-user shell/PATH configuration.

The release workflow builds signed/notarized production packages when the
required Apple and Azure signing secrets are configured. Dry-run releases can
build unsigned packages for validation. macOS releases include Intel, Apple
Silicon, and universal PKGs.

The Windows MSI is per-user: it installs under
`%USERPROFILE%\\.dcc-ai-tracking\\bin` and changes only that user's `PATH` (the MSI
owns the user PATH entry and removes it on uninstall; `install-hooks --env`
additionally configures Git Bash profiles when Git Bash is installed). It has
no all-users or Administrator install mode. By default, the MSI also installs
`dcc-ai-tracking` in eligible WSL distributions. Windows downloads the same
release from the enterprise npm registry using the distro's x64 or ARM64 Linux
package. WSL setup is best-effort and excludes Docker-managed `docker-desktop*`
distributions. Disable it for interactive or silent installs by passing
`INSTALL_WSL=0` to `msiexec`. The macOS PKG copies its bundled binary into the
active console user's `~/.dcc-ai-tracking/bin`, then runs setup as that user, including
adding `~/.dcc-ai-tracking/bin` to the user's shell PATH configs. It fails if no valid
console user is logged in or per-user setup fails.

For an enterprise endpoint, pass configuration to the MSI when installing:

```powershell
msiexec /i dcc-ai-tracking-windows-x64.msi API_BASE=https://usegitai.com API_KEY=your-api-key
```

To skip WSL installation, add `INSTALL_WSL=0` to that command.

These values configure only the installing user's Git AI config. They are
hidden from MSI logs, but command-line arguments can still be visible to local
process inspection and shell history. Use your endpoint-management secret
mechanism when available. When WSL installation is enabled, the API values are
also forwarded to each Linux installer through the `wsl.exe` child environment
and `WSLENV`; they are not added to the WSL process command line.
