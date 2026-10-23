# Native engine override

- Rust FFI/build integration: crates.io `whisper-rs-sys` 0.15.0 (Unlicense).
- Native sources: whisper.cpp **v1.9.5**, commit `d1be6fde11ac6e0407606b4e42fe72d34add8037` (MIT).
- Source: https://github.com/ggml-org/whisper.cpp/releases/tag/v1.9.5
- Downloaded GitHub archive SHA-256: `469e96407d4a5f5a72288160f3b5bf0ec69f218b09d68cee2d552a27b525a469`.

Only native library build inputs are included; upstream workflows, examples,
tests and models are omitted. `upstream-files.json` records every imported native
file hash. Upstream native sources are unmodified. Rust bindings are regenerated
with bindgen 0.72 from the pinned headers, including the Vulkan header.
Third-party comments and attribution are retained, not subject to application
comment cleanup.

The whisper.h changes from the crate's 1.8.3 are additive function declarations,
but ggml_backend_dev_caps gained mmap_support; old bindings must not be reused.
Native/Rust ABI regression
checks protect the structs passed by value, callback offsets and VAD parameters.
The application still uses whisper-rs 0.16.0 and its native cancellation workaround.

Local build integration changes: refresh the OUT_DIR source copy on input changes,
watch native sources/bindings, disable development-version suffixes, and parse
the 1.9.x CMake version declarations. Normal builds use bundled bindings and do
not download sources. For maintenance, WHISPER_REGENERATE_BINDINGS=1 invokes the
build.rs generator; its OUT_DIR/bindings.rs is the source for src/bindings.rs.

Native license: `whisper.cpp/LICENSE`. Rust FFI license: `LICENSE`.
