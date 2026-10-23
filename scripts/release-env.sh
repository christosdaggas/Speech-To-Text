#!/usr/bin/env bash
# Source from the repository root before a distributable release build.
# Copyright (C) 2026 Christos A. Daggas
# SPDX-License-Identifier: MIT

# Strip workstation paths from Rust panic locations and C/C++ __FILE__ strings.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=/build --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/build/cargo --remap-path-prefix=$PWD=/build/speech-to-text"
export CFLAGS="${CFLAGS:+$CFLAGS }-ffile-prefix-map=$HOME=/build -ffile-prefix-map=$PWD=/build/speech-to-text"
export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-ffile-prefix-map=$HOME=/build -ffile-prefix-map=$PWD=/build/speech-to-text"
