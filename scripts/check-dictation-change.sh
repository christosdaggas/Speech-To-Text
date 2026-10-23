#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export WHISPER_DONT_GENERATE_BINDINGS=1
cargo fmt --all -- --check
bash scripts/test-version.sh
python3 scripts/test-rpm-builder.py
python3 scripts/check-whisper-abi.py
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
git diff --check
echo DICTATION_CHECKS_PASSED
