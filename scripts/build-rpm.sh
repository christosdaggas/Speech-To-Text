#!/usr/bin/env bash
# Copyright (C) 2026 Christos A. Daggas
# SPDX-License-Identifier: MIT
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in cargo python3 rpmbuild rpmspec rpm msgfmt desktop-file-validate appstreamcli; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required; see CONTRIBUTING.md" >&2; exit 1; }
done
[ "$(uname -m)" = x86_64 ] || { echo "error: the Fedora RPM currently supports x86_64 only" >&2; exit 1; }

bash scripts/version.sh --check
VERSION=$(bash scripts/version.sh)
desktop-file-validate data/com.chrisdaggas.speech-to-text.desktop
appstreamcli validate --no-net data/com.chrisdaggas.speech-to-text.metainfo.xml

# Rebuild instead of packaging a stale binary. All release builds include Vulkan.
source scripts/release-env.sh
BIN=$(cargo build --offline --locked --release --features vulkan --message-format=json-render-diagnostics |
    python3 -c '
import json, sys
binary = None
for line in sys.stdin:
    message = json.loads(line)
    if (message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "speech-to-text"
            and "bin" in message.get("target", {}).get("kind", [])
            and message.get("executable")):
        binary = message["executable"]
if not binary:
    sys.exit("error: Cargo did not report the application binary")
print(binary)
')
[ "$("$BIN" --version)" = "Speech to Text $VERSION" ] || {
    echo "error: the binary version does not match Cargo.toml" >&2; exit 1;
}
[ "$("$BIN" --engine-version)" = "whisper.cpp 1.9.5" ] || {
    echo "error: unexpected native Whisper engine version" >&2; exit 1;
}
grep -qa ggml_vulkan "$BIN" || { echo "error: Vulkan backend is missing" >&2; exit 1; }
if grep -qaF "$HOME/" "$BIN"; then
    echo "error: the release binary still contains a workstation home path" >&2
    exit 1
fi

SPEC=speech-to-text.spec
TOP="$PWD/dist/rpmbuild"
OUT="$PWD/dist/rpm"
mkdir -p "$TOP"/{SOURCES,SPECS,BUILD,SRPMS,BUILDROOT} "$OUT"
SRC="$TOP/SOURCES"

install -m755 "$BIN" "$SRC/speech-to-text"
install -m644 data/com.chrisdaggas.speech-to-text.desktop "$SRC/"
install -m644 data/com.chrisdaggas.speech-to-text.metainfo.xml "$SRC/"
install -m644 data/resources/style.css "$SRC/"
install -m644 data/icons/hicolor/scalable/apps/com.chrisdaggas.speech-to-text.svg "$SRC/"
install -m644 data/icons/hicolor/scalable/apps/com.chrisdaggas.speech-to-text-ai.svg "$SRC/"
install -m644 data/icons/hicolor/symbolic/apps/com.chrisdaggas.speech-to-text-symbolic.svg "$SRC/"
install -m644 data/vad/ggml-silero-v5.1.2.bin "$SRC/"
install -m644 LICENSE "$SRC/"
install -m644 native/LICENSE-sherpa-onnx native/NOTICE "$SRC/"
install -m644 vendor/whisper-rs-sys/whisper.cpp/LICENSE "$SRC/LICENSE-whisper.cpp"
install -m644 vendor/whisper-rs-sys/LICENSE "$SRC/LICENSE-whisper-rs-sys"
mapfile -t langs < <(grep -v '^\s*#' po/LINGUAS | tr -s ' \n' '\n' | grep -v '^$')
for lang in "${langs[@]}"; do
    msgfmt -c "po/${lang}.po" -o "$SRC/${lang}.mo"
done
install -m644 "$SPEC" "$TOP/SPECS/$SPEC"

# Never publish the workstation hostname in RPM metadata.
rpmbuild -bb --define "_topdir $TOP" --define "_rpmdir $OUT" \
    --define '_rpmfilename %{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}.rpm' \
    --define "_buildhost localhost" "$TOP/SPECS/$SPEC"
NAME=$(rpmspec -q --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}.rpm' "$SPEC")
RPM="$OUT/$NAME"
[ "$(rpm -qp --qf '%{VERSION}' "$RPM")" = "$VERSION" ]
rpm -qp --requires "$RPM" | grep -q libvulkan
(cd "$OUT" && sha256sum "$NAME" > "$NAME.sha256")
echo "built: dist/rpm/$NAME"
