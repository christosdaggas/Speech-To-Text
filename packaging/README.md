# Fedora RPM packaging

Supported target: **Fedora 44, GNOME, x86_64**.

## Build the installation package

Install the requirements in [CONTRIBUTING.md](../CONTRIBUTING.md), then run:

```sh
bash scripts/build-rpm.sh
```

The script validates desktop metadata, builds the current release with Vulkan,
checks its version, and writes the RPM and SHA-256 checksum to **`dist/rpm/`**.
Builds use cached Cargo dependencies (`--offline --locked`); on a fresh checkout,
run `cargo fetch --locked` once beforehand. No installation, upload, signing or
GitHub Actions workflow is triggered.

Install the generated file with `sudo dnf install ./dist/rpm/<filename>.rpm`.
RPM Epoch 1 allows upgrades from older development packages numbered 1.7.x even
though the application now reports 1.0.0. Existing settings and history are kept.
Fedora file triggers refresh desktop entries and icon caches automatically.

## Source-build spec

`packaging/speech-to-text.spec` builds from a source archive; the root
`speech-to-text.spec` packages the binary produced by the local build script.
Both share the same version, dependencies and installed application assets.
The source archive URL requires a separately published matching GitHub tag;
the local RPM build does not need a tag or network access.
When publishing the reset 1.0.0 release, explicitly mark it as **Latest** on
GitHub so the update endpoint does not keep pointing to an older 1.7.x build.

No public signing key has been published. A checksum detects corruption but does
not authenticate the publisher; see [SECURITY.md](../SECURITY.md).
