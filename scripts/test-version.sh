#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp"/{scripts,packaging,data}
cp "$root/scripts/version.sh" "$tmp/scripts/"
printf '[package]\nversion = "1.0.0"\n' > "$tmp/Cargo.toml"
printf 'name = "speech-to-text"\nversion = "1.0.0"\n' > "$tmp/Cargo.lock"
printf 'Version: 1.0.0\n' > "$tmp/speech-to-text.spec"
cp "$tmp/speech-to-text.spec" "$tmp/packaging/"
printf '<release version="1.0.0"/>\n' > "$tmp/data/com.chrisdaggas.speech-to-text.metainfo.xml"
test "$(bash "$tmp/scripts/version.sh")" = 1.0.0
bash "$tmp/scripts/version.sh" --check

# Neither old tags nor a stale build environment may change the manifest version.
git -C "$tmp" init -q
git -C "$tmp" -c user.name=VersionTest -c user.email=info@chrisdaggas.com commit -q --allow-empty -m initial
git -C "$tmp" tag v9.8.7
test "$(SPEECH_TO_TEXT_VERSION=9.8.7 bash "$tmp/scripts/version.sh")" = 1.0.0
printf 'Version: 9.8.7\n' > "$tmp/speech-to-text.spec"
if bash "$tmp/scripts/version.sh" --check > /dev/null 2>&1; then
    echo "error: version drift was not rejected" >&2
    exit 1
fi
bash "$root/scripts/version.sh" --check
echo VERSION_CHECKS_PASSED
