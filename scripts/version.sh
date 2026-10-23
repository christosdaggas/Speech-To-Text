#!/usr/bin/env bash
# Copyright (C) 2026 Christos A. Daggas
# SPDX-License-Identifier: MIT
set -euo pipefail
cd "$(dirname "$0")/.."

# Cargo.toml is authoritative, including in source archives without Git metadata.
VERSION=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "error: Cargo.toml must contain a stable X.Y.Z version" >&2
    exit 1
}

case "${1:-}" in
    "") printf '%s\n' "$VERSION"; exit 0 ;;
    --check) ;;
    *) echo "usage: $0 [--check]" >&2; exit 1 ;;
esac

fail=0
check() {
    if [ "$2" != "$VERSION" ]; then
        echo "version drift: $1 says '$2', Cargo.toml says '$VERSION'" >&2
        fail=1
    fi
}
check "Cargo.lock" "$(awk '/^name = "speech-to-text"$/ { getline; gsub(/"/, ""); print $3; exit }' Cargo.lock)"
check "speech-to-text.spec" "$(awk '/^Version:/ { print $2; exit }' speech-to-text.spec)"
check "packaging/speech-to-text.spec" "$(awk '/^Version:/ { print $2; exit }' packaging/speech-to-text.spec)"
check "AppStream" "$(grep -m1 -o 'release version="[^"]*"' data/com.chrisdaggas.speech-to-text.metainfo.xml | cut -d'"' -f2)"
[ "$fail" -eq 0 ] || exit 1
echo "OK: all static versions agree with $VERSION"
