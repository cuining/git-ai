# Git AI packaging

This directory contains MSI and PKG installer scaffolding for Git AI.

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
no all-users or Administrator install mode. By default, the MSI also installs
the same Git AI release into every WSL distribution except Docker-managed
`docker-desktop*` distributions. WSL setup is best-effort and does not fail the
Windows installation. Disable it for interactive or silent installs by passing
`INSTALL_WSL=0` to `msiexec`. The macOS PKG copies its bundled binary into the
active console user's `~/.git-ai/bin`, then runs setup as that user, including
adding `~/.git-ai/bin` to the user's shell PATH configs. It fails if no valid
console user is logged in or per-user setup fails.

For an enterprise endpoint, pass configuration to the MSI when installing:

```powershell
msiexec /i git-ai-windows-x64.msi API_BASE=https://dcc-service.intra.xiaojukeji.com/api/v1/ai-tracking API_KEY=your-api-key
```

To skip WSL installation, add `INSTALL_WSL=0` to that command.

These values configure only the installing user's Git AI config. They are
hidden from MSI logs, but command-line arguments can still be visible to local
process inspection and shell history. Use your endpoint-management secret
mechanism when available. When WSL installation is enabled, the API values are
also forwarded to each Linux installer through the `wsl.exe` child environment
and `WSLENV`; they are not added to the WSL process command line.

## Fork updates through an Artifactory npm repository

Set `GIT_AI_NPM_UPDATE_PACKAGE_URL` in the environment **when building** the
release binary. Use the npm registry URL followed by the package prefix,
including the literal slash for a scoped name. For example:

```sh
GIT_AI_NPM_UPDATE_PACKAGE_URL='https://artifactory.intra.xiaojukeji.com/artifactory/api/npm/npm-dev-local/@your-scope/git-ai-fork' \
  ./build.sh macos-arm64
```

The binary requests `@your-scope/git-ai-fork-darwin-arm64/latest` on macOS ARM64.
Collect the binaries from `build.sh` with their adjacent `.version` files.
Use the same package prefix with `scripts/stage-npm.sh` to prepare publish
directories. It reads `output/` by default, rejects binaries whose version file
is missing or differs from `Cargo.toml`, and writes packages to `output/npm/`
or another new or empty output directory.

Publish to a repository that the client can read over HTTPS. The
existing `disable_version_checks` and `disable_auto_updates` settings can
override the defaults. Install an npm-enabled build once to switch an existing
installation to this source. Subsequent releases must use a higher Cargo
version and be built with the same source configured.
