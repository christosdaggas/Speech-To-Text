# Security Policy

This is a small, single-maintainer personal project, provided as-is. Security
fixes are made on a best-effort basis for the latest release.

## Reporting a vulnerability

Please **do not** open a public issue for security problems. Instead, either:

- use GitHub's **private vulnerability reporting**
  (Security → "Report a vulnerability"), or
- email **info@chrisdaggas.com** with the subject
  `SECURITY: speech-to-text`.

Please include the app version and steps to reproduce. I'll look into it and fix
what I can, but as a spare-time project I can't promise a fixed timeline.

## Verifying downloads

Local RPM builds write a `.rpm.sha256` file beside the package in `dist/rpm/`.
From that directory, check downloaded bytes with:

```sh
sha256sum --check speech-to-text-*.rpm.sha256
```

The RPM is currently unsigned. A checksum detects corruption, but does not
authenticate the publisher. No public GPG signing key has been published yet.
If a future release includes `SHA256SUMS`, `SHA256SUMS.asc` and the matching public
key in `KEYS`, verify both the signature and checksums with:

```sh
./scripts/verify-release.sh
```
