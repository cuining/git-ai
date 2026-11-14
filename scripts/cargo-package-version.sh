cargo_package_version() {
    cargo_package_field "$1" version
}

cargo_package_field() {
    local value
    value="$(awk -v field="$2" '
        /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/) }
        in_package && $0 ~ ("^[[:space:]]*" field "[[:space:]]*=") {
            split($0, fields, "\"")
            print fields[2]
            exit
        }
    ' "$1")" || return 1
    if [[ -z "$value" ]]; then
        echo "Missing package $2 in $1" >&2
        return 1
    fi
    printf '%s\n' "$value"
}
