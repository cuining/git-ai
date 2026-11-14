#!/bin/bash

set -euo pipefail

# Parse arguments
BUILD_TYPE="debug"
if [[ "$#" -gt 0 && "$1" == "--release" ]]; then
    BUILD_TYPE="release"
fi

# Clean up old dev-symlinks.sh PATH export if present
_detect_shell_profile() {
    if [[ "${SHELL:-}" == */zsh ]]; then
        if [[ -f "$HOME/.zshrc" ]]; then
            echo "$HOME/.zshrc"
        else
            echo "$HOME/.zprofile"
        fi
    elif [[ "${SHELL:-}" == */bash ]]; then
        if [[ "$(uname)" == "Darwin" ]]; then
            if [[ -f "$HOME/.bash_profile" ]]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.bashrc"
            fi
        else
            if [[ -f "$HOME/.bashrc" ]]; then
                echo "$HOME/.bashrc"
            else
                echo "$HOME/.bash_profile"
            fi
        fi
    else
        echo "$HOME/.profile"
    fi
}

_PROFILE="$(_detect_shell_profile)"
if [[ -f "$_PROFILE" ]] && grep -q '\.git-ai-local-dev/gitwrap/bin' "$_PROFILE"; then
    sed -i.bak '/# git-ai local dev/d' "$_PROFILE"
    sed -i.bak '/\.git-ai-local-dev\/gitwrap\/bin/d' "$_PROFILE"
    rm -f "$_PROFILE.bak"
    echo "Cleaned up old git-ai local dev PATH export from $_PROFILE"
fi

# Run the production installer if the install directory is missing from the home or shell profile.
INSTALL_ROOT_NAME=".git-ai"
INSTALL_BIN_PATH="$INSTALL_ROOT_NAME/bin"
# shellcheck source=scripts/cargo-package-version.sh
source "$(dirname "${BASH_SOURCE[0]}")/cargo-package-version.sh"
EXECUTABLE_NAME="$(cargo_package_field Cargo.toml default-run)"
INSTALLED_BINARY="$HOME/$INSTALL_BIN_PATH/$EXECUTABLE_NAME"
if [[ ! -d "$HOME/$INSTALL_BIN_PATH" ]] || [[ ! -f "$HOME/$INSTALL_ROOT_NAME/config.json" ]] || \
   { [[ -f "$_PROFILE" ]] && ! grep -qF "$INSTALL_BIN_PATH" "$_PROFILE"; } || \
   { [[ ! -f "$_PROFILE" ]]; }; then
    echo "Running git-ai installer..."
    curl -sSL https://usegitai.com/install.sh | bash
fi

# Build the binary
echo "Building $BUILD_TYPE binary..."
if [[ "$BUILD_TYPE" == "release" ]]; then
    cargo build --release
else
    cargo build
fi

# Install binary via temp file + atomic mv to avoid macOS code signature cache
# issues: direct cp reuses the inode, causing syspolicyd to fail validating the
# changed binary, leaving the process stuck in launched-suspended state unkillably.
echo "Installing binary to ~/$INSTALL_BIN_PATH/$EXECUTABLE_NAME..."
TMP_BIN="$INSTALLED_BINARY.tmp.$$"
cp "target/$BUILD_TYPE/$EXECUTABLE_NAME" "$TMP_BIN"
mv -f "$TMP_BIN" "$INSTALLED_BINARY"
chmod +x "$INSTALLED_BINARY"

# Run install hooks
echo "Running install hooks..."
"$INSTALLED_BINARY" install

echo "Done!"
