cargo_package_version() {
    local version
    version="$(awk '
        /^\[/ { in_package = ($0 == "[package]") }
        in_package && /^version[[:space:]]*=/ {
            split($0, fields, "\"")
            print fields[2]
            exit
        }
    ' "$1")" || return 1
    if [[ -z "$version" ]]; then
        echo "Missing package version in $1" >&2
        return 1
    fi
    printf '%s\n' "$version"
}
