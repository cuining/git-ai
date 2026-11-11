#!/bin/bash
set -euo pipefail

cd -- "${HOME:?WSL user home is unavailable}"
staging_dir=$(mktemp -d)
trap 'rm -rf -- "$staging_dir"' EXIT

archive="$staging_dir/payload.tar"
binary="$staging_dir/git-ai"
installer="$staging_dir/install.sh"
unset TAR_OPTIONS
cat > "$archive"
tar -xf "$archive" -C "$staging_dir" -- git-ai install.sh

for path in "$binary" "$installer"; do
    if [[ ! -f "$path" || -L "$path" || ! -s "$path" ]]; then
        printf '%s\n' 'The WSL installation payload is incomplete.' >&2
        exit 1
    fi
done

# Tar can accept missing EOF blocks. The host writes two fixed headers,
# 512-byte-aligned file contents and exactly two EOF blocks.
archive_size=$(wc -c < "$archive")
binary_size=$(wc -c < "$binary")
installer_size=$(wc -c < "$installer")
expected_size=$((2048 + (binary_size + 511) / 512 * 512 + (installer_size + 511) / 512 * 512))
if (( archive_size != expected_size )); then
    printf '%s\n' 'The WSL installation payload is incomplete.' >&2
    exit 1
fi

unset GIT_AI_NPM_UPGRADE GIT_AI_INSTALL_USER INSTALL_NONCE
# The packaged checksum uses the npm installer's canonical asset name.
GIT_AI_NPM_UPGRADE=1 GIT_AI_LOCAL_BINARY="$binary" bash "$installer" </dev/null

installed_binary="$HOME/.git-ai/bin/git-ai"
local_bin="$HOME/.local/bin"
if ! { mkdir -p "$local_bin" && [[ ! -d "$local_bin/git-ai" ]] &&
    ln -sfn "$installed_binary" "$local_bin/git-ai"; } 2>/dev/null; then
    printf '%s\n' 'Warning: could not link git-ai into ~/.local/bin.' >&2
fi

# Mirror the packaged installer's handling of distros whose default user is root.
if (( EUID == 0 )); then
    export GIT_AI_ALLOW_SUPERUSER=1
fi
if ! "$installed_binary" install-hooks --env </dev/null; then
    printf '%s\n' 'Installed git-ai in WSL, but hook setup failed.' >&2
    exit 1
fi
