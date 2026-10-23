#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export WHISPER_DONT_GENERATE_BINDINGS=1
source scripts/release-env.sh
cargo build --offline --locked --release --features vulkan
cargo test --offline --locked --release --features vulkan
echo VULKAN_RELEASE_CHECKS_PASSED
