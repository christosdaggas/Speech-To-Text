# Contributing

Thanks for your interest in improving Speech to Text!

## Development setup

Install the system dependencies on Fedora 44:

```
sudo dnf install gtk4-devel libadwaita-devel alsa-lib-devel cmake clang gcc-c++ \
    gettext glib2-devel desktop-file-utils appstream rpm-build vulkan-loader-devel glslc python3
```

The supported compiler baseline is Rust 1.99 (Linux x86_64); older compilers
are not part of the verified support matrix. Install that or a newer stable
toolchain with `rustup`, then add the linting components:

```sh
rustup component add rustfmt clippy
```

Build and test (whisper-rs ships pregenerated bindings; this skips bindgen):

```sh
export WHISPER_DONT_GENERATE_BINDINGS=1
cargo build
cargo test
./target/release/speech-to-text   # after `cargo build --release`
```

GNOME portals require the installed desktop entry to identify the application.
For global shortcuts and auto-paste, install the RPM and relaunch from GNOME's
app launcher. A bare binary without that desktop entry still supports in-app
recording; it cannot register its portal app ID.

## Before you open a PR

Run the quality checks locally:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Supply-chain checks (install once: `cargo install cargo-deny cargo-audit`):

```sh
cargo audit
cargo deny check
```

Optional hygiene tools:

```sh
cargo install cargo-machete && cargo machete   # unused dependencies
cargo tree -d                                  # duplicate versions
```

## Coding guidelines

- Match the surrounding style. Keep comments that explain safety or intent;
  avoid narrating obvious code.
- **Never** log transcript text or secrets. Route user-facing error text through
  `error::redact_secrets` / `AppError::user_message`.
- Write local state via `fsio::write_private` (atomic, 0600). Store secrets only
  via the `secrets` module (keyring).
- Validate any externally-influenced path (`transcription::safe_path`) and verify
  any download (`transcription::verify`) before use.
- Add tests for security-relevant behavior; see existing tests in
  `archive.rs`, `safe_path.rs`, `verify.rs`, `fsio.rs`, `llm.rs`, `config.rs`.

## Versioning

`Cargo.toml` is the single source of the application version. The interface,
`--version` and local API use `CARGO_PKG_VERSION`; packaging reads the same value
through `scripts/version.sh`. Git tags and environment overrides cannot change it.
When releasing, update the two RPM specs, AppStream release notes and changelog,
and let Cargo update `Cargo.lock`. Run `bash scripts/test-version.sh` to catch drift.
Historical changelog versions and third-party runtime versions are not app versions.

Build the supported Vulkan-enabled Fedora package with:

```sh
bash scripts/build-rpm.sh
```

It builds the current sources, verifies the binary version and writes the RPM
directly to `dist/rpm/`. It does not install or publish anything. See
[packaging/README.md](packaging/README.md) for package details.
The binary path comes from Cargo's artifact output, including custom target directories.

## Maintenance scripts

| Script | Purpose |
| --- | --- |
| `build-rpm.sh` | Build and package the Fedora release in `dist/rpm/`. |
| `version.sh`, `test-version.sh` | Read the manifest version and test release consistency. |
| `test-rpm-builder.py` | Test Cargo artifact selection, custom target paths and fixed RPM output paths. |
| `check-whisper-abi.py` | Verify pinned native source hashes and C/Rust FFI layouts. |
| `check-dictation-change.sh` | Local formatting, lint, test and default-build checks. |
| `check-vulkan-release.sh` | Release build and tests with the supported Vulkan backend. |
| `release-env.sh` | Shared build environment that removes workstation paths from release binaries. |
| `update-translations.sh` | Extract and merge gettext catalogs after UI changes. |
| `update-pins.sh` | Fetch candidate download hashes for manual review; never runs automatically. |
| `verify-release.sh` | Verify signatures and checksums when a release signing key is published. |
| `benchmark.py` | Optional synthetic English/Greek benchmark generation and scoring. |

The unused Debian package builder has been retired; Fedora RPM is the supported format.

Downloaded runtimes and model files verify against pinned SHA-256 hashes in
`src/transcription/verify.rs`, anchored to immutable HuggingFace commits.
After a deliberate artifact update, run `scripts/update-pins.sh` and review
the new pins as part of the change.

## Warm model lifetimes

Whisper uses a local `whisper-rs-sys` override containing pinned whisper.cpp 1.9.5
and regenerated bindings. No sources are downloaded during builds. See
[`vendor/whisper-rs-sys/UPSTREAM.md`](vendor/whisper-rs-sys/UPSTREAM.md) for provenance
and binding regeneration. Run `python3 scripts/check-whisper-abi.py` after native
changes; `speech-to-text --engine-version` reports the linked engine without GTK
or configuration initialization. App and package versions remain independent.

Parakeet lazily starts a private instance of the application in worker mode and
loads the downloaded, pinned sherpa-onnx 1.13.8 shared library once. An anonymous
Unix socket carries bounded 16-bit PCM and JSON; no TCP listener, Python runtime,
temporary audio file or transcript logging is used. Each clip creates/destroys a
fresh native recognition stream. Matching model/runtime file identities and thread
counts reuse the worker; cancellation, errors, deadlines, backend switches, model
deletion and app shutdown terminate/reap it. Resident model memory is the explicit
tradeoff for removing repeat startup costs. The next transcription reloads after
termination; the UI thread never holds the inference/request lock.

Whisper's CPU VAD context is shared under one worker-side mutex. The pinned
whisper.cpp `whisper_vad_detect_speech` resets recurrent state and probabilities
for every clip. Missing/changed files and inference errors invalidate the context;
lock waiting respects cancellation. Model weights remain cached across Whisper
recordings, independent of which ASR model is selected.

`native/parakeet-abi.h` contains the offline ABI declarations from the pinned
runtime (Apache-2.0, with notices bundled in the RPM). `native/parakeet.c` exposes
an opaque, ownership-paired interface to Rust and rejects other runtime versions.
The `cc` build dependency was already in the dependency graph; no new runtime
crate or build-time model download is required. A deliberate runtime version
upgrade must also review the ABI declarations and native adapter.

## Security issues

Please do **not** file public issues for vulnerabilities — see
[SECURITY.md](SECURITY.md).

## CI

This project runs on a fork-and-test basis: there are no GitHub Actions
workflows, so contributors and maintainers verify their changes with the
commands above.

`bash scripts/check-dictation-change.sh` includes the unit/regression tests.
For the shipping Linux GPU build also run `bash scripts/check-vulkan-release.sh`.
Do not use `--all-features`: CUDA/Metal are separate platform configurations.
`build.rs` generates translations under Cargo's `OUT_DIR`; packaging compiles
the `.po` sources explicitly. A build must not modify tracked `.mo` files.
An offline `cargo deny --offline check` uses cached advisories; refresh the
advisory database for the release security check.
