// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Hardened ZIP extraction for downloaded runtime archives. Uses the zip
//! crate's `enclosed_name()` for path-traversal safety, enforces entry-count
//! and total-decompressed-size caps (zip-bomb guard), rejects special files,
//! and validates symlink targets stay inside the destination (libtorch ships
//! legitimate symlinks, so we permit contained ones rather than failing).

use crate::error::{AppError, AppResult};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Max entries in a runtime archive. The Cohere runtime bundles a full libtorch
/// tree (cmake configs, headers, per-op sources), which alone is ~9000 files —
/// the previous 8192 cap rejected the legitimate release. The real zip-bomb
/// defense is [`MAX_TOTAL_BYTES`] plus per-entry path/size validation; the
/// entry count only guards against pathological metadata blow-up, so a headroom
/// ceiling is enough.
const MAX_ENTRIES: usize = 32_768;
/// Max total decompressed bytes (zip-bomb guard). Runtimes bundle libtorch, so
/// allow a generous ceiling but still bounded.
const MAX_TOTAL_BYTES: u64 = 6 * 1024 * 1024 * 1024; // 6 GiB

/// Extract `zip_path` into `dest` safely. Caller should verify the archive hash
/// (see `verify`) before calling this.
pub fn safe_extract_zip(zip_path: &Path, dest: &Path) -> AppResult<()> {
    // Never extract through pre-existing parents or links. Work in an owned,
    // private directory; publish only after every link has been resolved.
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if !meta.is_dir() || std::fs::read_dir(dest)?.next().is_some() {
            return Err(AppError::ModelDownloadFailed(
                "Extraction destination must be empty and not a symlink".into(),
            ));
        }
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    extract_zip_into(zip_path, staging.path())?;
    if dest.exists() {
        std::fs::remove_dir(dest)?;
    }
    std::fs::rename(staging.path(), dest)?;
    Ok(())
}

fn extract_zip_into(zip_path: &Path, dest: &Path) -> AppResult<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::ModelDownloadFailed(format!("Failed to open zip: {e}")))?;

    if archive.len() > MAX_ENTRIES {
        return Err(AppError::ModelDownloadFailed(format!(
            "Archive has too many entries ({})",
            archive.len()
        )));
    }
    std::fs::create_dir_all(dest)?;

    let mut total: u64 = 0;
    let mut links = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::ModelDownloadFailed(format!("Zip read error: {e}")))?;

        // Path-traversal-safe relative name (rejects absolute/`..`).
        let Some(rel) = entry.enclosed_name() else {
            return Err(AppError::ModelDownloadFailed(format!(
                "Unsafe path in archive: {}",
                entry_label(&entry, i)
            )));
        };
        let out = dest.join(&rel);
        ensure_plain_parents(dest, &rel)?;
        if !out.starts_with(dest) {
            return Err(AppError::ModelDownloadFailed(format!(
                "Archive entry escapes destination: {}",
                rel.display()
            )));
        }

        let mode = entry.unix_mode().unwrap_or(0);
        let ftype = mode & 0o170000;

        // Symlink: validate the (lexically-resolved) target stays inside dest.
        if ftype == 0o120000 {
            if entry.size() > 4096 {
                return Err(AppError::ModelDownloadFailed(
                    "Archive symlink target too long".into(),
                ));
            }
            let mut target = String::new();
            entry
                .by_ref()
                .take(4097)
                .read_to_string(&mut target)
                .map_err(|e| {
                    AppError::ModelDownloadFailed(format!("Bad symlink in archive: {e}"))
                })?;
            if target.len() > 4096 {
                return Err(AppError::ModelDownloadFailed(
                    "Archive symlink target too long".into(),
                ));
            }
            let link_parent = out.parent().unwrap_or(dest);
            if !lexically_within(dest, link_parent, target.trim()) {
                return Err(AppError::ModelDownloadFailed(format!(
                    "Archive symlink escapes destination: {} -> {}",
                    rel.display(),
                    target.trim()
                )));
            }
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            links.push((out, target));
            continue;
        }

        // Reject anything that isn't a regular file or directory.
        if ftype != 0 && ftype != 0o040000 && ftype != 0o100000 {
            return Err(AppError::ModelDownloadFailed(format!(
                "Archive contains a special file: {}",
                rel.display()
            )));
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }

        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err(AppError::ModelDownloadFailed(
                "Archive decompressed size exceeds the safety limit".into(),
            ));
        }

        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out)?;
        let expected = entry.size();
        let copied = std::io::copy(&mut entry.take(expected.saturating_add(1)), &mut out_file)?;
        if copied != expected {
            return Err(AppError::ModelDownloadFailed(
                "Archive entry size mismatch".into(),
            ));
        }

        // Preserve executable bit for regular files (binaries inside the zip).
        #[cfg(unix)]
        if ftype == 0o100000 && (mode & 0o111) != 0 {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
        }
    }
    // No writes occur through symlinks: install them last, then resolve the
    // complete graph (including chains, cycles and parent traversal).
    #[cfg(unix)]
    for (out, target) in &links {
        std::os::unix::fs::symlink(target, out)?;
    }
    let root = dest.canonicalize()?;
    for (out, _) in &links {
        let resolved = out.canonicalize()?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err(AppError::ModelDownloadFailed(
                "Archive symlink does not resolve to a contained regular file".into(),
            ));
        }
    }
    Ok(())
}

fn ensure_plain_parents(root: &Path, relative: &Path) -> AppResult<()> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            if !matches!(component, Component::Normal(_) | Component::CurDir) {
                return Err(AppError::ModelDownloadFailed("Unsafe archive path".into()));
            }
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if !meta.is_dir() => {
                    return Err(AppError::ModelDownloadFailed(
                        "Archive parent is not a plain directory".into(),
                    ))
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&current)?
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Replace a runtime only after validation; restore the old tree if publication
/// fails. The backup stays beside the runtime until the new tree is installed.
pub fn publish_runtime(staged: &Path, dest: &Path) -> AppResult<()> {
    let backup = dest.with_extension(format!("previous-{}", uuid::Uuid::new_v4()));
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &backup)?;
    }
    if let Err(error) = std::fs::rename(staged, dest) {
        if had_old {
            std::fs::rename(&backup, dest)?;
        }
        return Err(error.into());
    }
    if had_old {
        if let Err(error) = std::fs::remove_dir_all(&backup) {
            tracing::warn!("Could not remove previous runtime backup: {error}");
        }
    }
    Ok(())
}

/// Extract a verified tar.bz2 into a fresh staging directory. Only regular files
/// and directories are accepted, with bounded size/count and contained paths.
pub fn safe_extract_tar_bz2(path: &Path, dest: &Path) -> AppResult<()> {
    let input = bzip2::read::BzDecoder::new(std::fs::File::open(path)?);
    let mut archive = tar::Archive::new(input);
    let mut total = 0u64;
    for (index, entry) in archive.entries()?.enumerate() {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let rel = entry.path()?.into_owned();
        if index >= MAX_ENTRIES
            || !(kind.is_file() || kind.is_dir())
            || rel
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(AppError::ModelDownloadFailed(
                "Unsafe runtime archive entry".into(),
            ));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES || !entry.unpack_in(dest)? {
            return Err(AppError::ModelDownloadFailed(
                "Runtime archive exceeds limits".into(),
            ));
        }
    }
    Ok(())
}

/// Human-readable label for an entry in error messages. zip 9 returns the raw
/// name as a `Result` (it fails for non-UTF-8 names), so fall back to the index.
fn entry_label(entry: &zip::read::ZipFile<'_, impl std::io::Read>, index: usize) -> String {
    entry
        .name()
        .map(|n| n.into_owned())
        .unwrap_or_else(|_| format!("entry #{index} (non-UTF-8 name)"))
}

/// Resolve `target` lexically relative to `base` and confirm it stays within
/// `root`. Rejects absolute targets. No filesystem access (target may not exist).
fn lexically_within(root: &Path, base: &Path, target: &str) -> bool {
    if target.is_empty() || Path::new(target).is_absolute() || target.contains('\\') {
        return false;
    }
    let mut stack: Vec<Component> = base.components().collect();
    for comp in Path::new(target).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            Component::Normal(_) => stack.push(comp),
            // Absolute/prefix components are unsafe.
            _ => return false,
        }
    }
    let resolved: PathBuf = stack.iter().collect();
    resolved.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_runtime_rejects_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.tar.bz2");
        let writer = bzip2::write::BzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            bzip2::Compression::fast(),
        );
        let mut archive = tar::Builder::new(writer);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        archive
            .append_link(&mut header, "escape", "../outside")
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        assert!(safe_extract_tar_bz2(&path, &dest).is_err());
        assert!(!dest.join("escape").exists());
    }
    use std::io::Write;
    use std::path::Path;

    /// Write a ZIP at `path` from (name, bytes) entries, stored (no compression).
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    #[test]
    fn extracts_a_safe_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ok.zip");
        write_zip(&zip_path, &[("bin/tool", b"hello"), ("data/x.txt", b"y")]);

        let dest = tmp.path().join("out");
        safe_extract_zip(&zip_path, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("bin/tool")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dest.join("data/x.txt")).unwrap(), b"y");
    }

    #[test]
    fn rejects_too_many_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("many.zip");
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        write_zip(&zip_path, &entries);

        let dest = tmp.path().join("out");
        let err = safe_extract_zip(&zip_path, &dest).unwrap_err();
        assert!(err.to_string().contains("too many entries"), "got: {err}");
    }

    #[test]
    fn traversal_entry_does_not_escape_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil.zip");
        // A path-traversal entry plus a benign one.
        write_zip(
            &zip_path,
            &[("../escaped.txt", b"pwned"), ("safe.txt", b"ok")],
        );

        let dest = tmp.path().join("out");
        let _ = safe_extract_zip(&zip_path, &dest); // may Err or skip; must not escape

        // The crucial invariant: nothing was written outside `dest`.
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "traversal entry escaped the destination directory"
        );
    }

    #[test]
    fn symlink_target_containment() {
        let root = Path::new("/rt");
        let base = Path::new("/rt/libtorch/lib");
        assert!(lexically_within(root, base, "libc10.so.1")); // same dir
        assert!(lexically_within(root, base, "../lib/libc10.so")); // sibling, still inside
        assert!(!lexically_within(root, base, "../../../etc/passwd")); // escapes
        assert!(!lexically_within(root, base, "/etc/passwd")); // absolute
        assert!(!lexically_within(root, base, "")); // empty
    }

    #[test]
    #[cfg(unix)]
    fn zip_links_are_validated_as_a_graph_before_publication() {
        for unsafe_link in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("runtime.zip");
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("lib/real.so", opts).unwrap();
            zip.write_all(b"library").unwrap();
            zip.add_symlink("lib/alias.so", "real.so", opts).unwrap();
            if unsafe_link {
                zip.add_symlink("parent", ".", opts).unwrap();
                zip.add_symlink("escape", "parent/../outside", opts)
                    .unwrap();
            }
            zip.finish().unwrap();
            let dest = tmp.path().join("out");
            let result = safe_extract_zip(&path, &dest);
            assert_eq!(result.is_err(), unsafe_link);
            if unsafe_link {
                assert!(!dest.exists());
            } else {
                assert_eq!(
                    std::fs::read(dest.join("lib/alias.so")).unwrap(),
                    b"library"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn zip_refuses_existing_symlink_parents_and_preserves_old_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&dest).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("lib")).unwrap();
        let zip = tmp.path().join("runtime.zip");
        write_zip(&zip, &[("lib/escape", b"bad")]);
        assert!(safe_extract_zip(&zip, &dest).is_err());
        assert!(!outside.join("escape").exists());
        assert!(publish_runtime(&tmp.path().join("missing"), &dest).is_err());
        assert!(dest.join("lib").symlink_metadata().unwrap().is_symlink());
    }
}
