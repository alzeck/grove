//! Copying `copy_from_original` paths (`.env`, `node_modules`, …) from a main
//! clone into a new worktree. On APFS this uses `clonefile(2)`, which makes a
//! copy-on-write clone of a whole directory tree almost instantly.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyReport {
    /// Paths relative to both roots.
    pub copied: Vec<PathBuf>,
    /// Matches whose destination already existed; they are left untouched.
    pub skipped_existing: Vec<PathBuf>,
    /// Patterns that matched nothing in the source.
    pub missing_patterns: Vec<String>,
    /// Whether at least one path was copied with `clonefile`.
    pub used_clonefile: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("invalid copy pattern `{pattern}`: {reason}")]
    InvalidPattern { pattern: String, reason: String },
    #[error("copying {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Copies everything matching `patterns` (paths or globs relative to
/// `src_root`) to the same relative location under `dst_root`. Existing
/// destinations are never overwritten. A matched symlink is followed;
/// symlinks inside copied directories are copied as symlinks.
pub async fn copy_from_original(
    src_root: &Path,
    dst_root: &Path,
    patterns: &[String],
) -> Result<CopyReport, CopyError> {
    let (src, dst, pats) = (src_root.to_owned(), dst_root.to_owned(), patterns.to_vec());
    match tokio::task::spawn_blocking(move || copy_blocking(&src, &dst, &pats)).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(CopyError::Io {
            path: src_root.to_owned(),
            source: io::Error::other(e),
        }),
    }
}

fn copy_blocking(
    src_root: &Path,
    dst_root: &Path,
    patterns: &[String],
) -> Result<CopyReport, CopyError> {
    let mut report = CopyReport::default();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let matches = find_matches(src_root, pattern)?;
        if matches.is_empty() {
            report.missing_patterns.push(pattern.clone());
        }
        for rel in matches {
            if !seen.insert(rel.clone()) {
                continue;
            }
            let dst = dst_root.join(&rel);
            if dst.symlink_metadata().is_ok() {
                report.skipped_existing.push(rel);
                continue;
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(at(parent))?;
            }
            report.used_clonefile |= copy_item(&src_root.join(&rel), &dst)?;
            report.copied.push(rel);
        }
    }
    Ok(report)
}

/// Paths under `src_root` matching `pattern`, relative to `src_root`.
fn find_matches(src_root: &Path, pattern: &str) -> Result<Vec<PathBuf>, CopyError> {
    let invalid = |reason: &str| CopyError::InvalidPattern {
        pattern: pattern.to_string(),
        reason: reason.to_string(),
    };
    let trimmed = pattern.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid("pattern is empty"));
    }
    if Path::new(trimmed)
        .components()
        .any(|c| matches!(c, Component::RootDir | Component::ParentDir))
    {
        return Err(invalid("must be relative and stay inside the checkout"));
    }
    let root = glob::Pattern::escape(&src_root.to_string_lossy());
    let full = format!("{}/{trimmed}", root.trim_end_matches('/'));
    let paths = glob::glob(&full).map_err(|e| invalid(e.msg))?;
    let mut matches = Vec::new();
    for entry in paths {
        let path = entry.map_err(|e| CopyError::Io {
            path: e.path().to_owned(),
            source: e.into(),
        })?;
        if let Ok(rel) = path.strip_prefix(src_root)
            && rel.components().any(|c| matches!(c, Component::Normal(_)))
        {
            matches.push(rel.to_owned());
        }
    }
    Ok(matches)
}

/// Copies one matched path; returns whether `clonefile` did it.
fn copy_item(src: &Path, dst: &Path) -> Result<bool, CopyError> {
    #[cfg(target_os = "macos")]
    match clonefile(src, dst) {
        Ok(()) => return Ok(true),
        // Not APFS, or a different volume.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV)) => {
            tracing::debug!("clonefile unsupported for {}: {e}", dst.display());
        }
        Err(e) => return Err(at(src)(e)),
    }
    copy_recursive(src, dst, true)?;
    Ok(false)
}

#[cfg(target_os = "macos")]
fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated C strings.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A plain recursive copy keeping permissions. Symlinks are recreated, not
/// followed, except `src` itself when `follow` is set (matching `clonefile`).
/// Sockets, FIFOs and devices are skipped.
fn copy_recursive(src: &Path, dst: &Path, follow: bool) -> Result<(), CopyError> {
    let meta = if follow {
        fs::metadata(src).or_else(|_| fs::symlink_metadata(src))
    } else {
        fs::symlink_metadata(src)
    }
    .map_err(at(src))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(src).map_err(at(src))?;
        std::os::unix::fs::symlink(target, dst).map_err(at(dst))?;
    } else if kind.is_dir() {
        fs::create_dir(dst).map_err(at(dst))?;
        for entry in fs::read_dir(src).map_err(at(src))? {
            let entry = entry.map_err(at(src))?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()), false)?;
        }
        // Last, in case the directory is read-only.
        fs::set_permissions(dst, meta.permissions()).map_err(at(dst))?;
    } else if kind.is_file() {
        fs::copy(src, dst).map_err(at(src))?;
    }
    Ok(())
}

fn at(path: &Path) -> impl FnOnce(io::Error) -> CopyError + '_ {
    move |source| CopyError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A main clone with the usual suspects.
    fn source_tree(root: &Path) {
        write(root, ".env", "SECRET=1\n");
        fs::set_permissions(root.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
        write(root, "node_modules/pkg/index.js", "module.exports = 1\n");
        write(root, "node_modules/pkg/cli.sh", "#!/bin/sh\n");
        fs::set_permissions(
            root.join("node_modules/pkg/cli.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        symlink("../pkg/cli.sh", root.join("node_modules/.bin/pkg")).unwrap();
        write(root, "apps/web/node_modules/a.js", "a");
        write(root, "apps/admin/node_modules/b.js", "b");
        write(root, "apps/docs/README.md", "no node_modules here");
        write(root, "shared/.env.shared", "SHARED=1\n");
        symlink("shared/.env.shared", root.join(".env.local")).unwrap();
    }

    fn strings(paths: &[PathBuf]) -> Vec<&str> {
        paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn copies_files_dirs_and_globs() {
        let tmp = TempDir::new().unwrap();
        let (src, dst) = (tmp.path().join("main"), tmp.path().join("wt"));
        source_tree(&src);
        write(&dst, ".env", "MINE=1\n");

        let patterns: Vec<String> = [
            ".env",
            ".env.local",
            "node_modules/",
            "apps/*/node_modules",
            "node_modules",
            "missing.txt",
            "build/*",
        ]
        .map(String::from)
        .to_vec();
        let report = copy_from_original(&src, &dst, &patterns).await.unwrap();

        assert_eq!(
            strings(&report.copied),
            [
                ".env.local",
                "node_modules",
                "apps/admin/node_modules",
                "apps/web/node_modules"
            ]
        );
        assert_eq!(strings(&report.skipped_existing), [".env"]);
        assert_eq!(report.missing_patterns, ["missing.txt", "build/*"]);
        assert_eq!(report.used_clonefile, cfg!(target_os = "macos"));

        assert_eq!(fs::read_to_string(dst.join(".env")).unwrap(), "MINE=1\n");
        // A matched symlink is followed.
        assert!(!dst.join(".env.local").is_symlink());
        assert_eq!(
            fs::read_to_string(dst.join(".env.local")).unwrap(),
            "SHARED=1\n"
        );
        assert_tree_copied(&src, &dst);
        assert_eq!(
            fs::read_to_string(dst.join("apps/web/node_modules/a.js")).unwrap(),
            "a"
        );
        assert!(!dst.join("apps/docs").exists());

        // Running again copies nothing new.
        let again = copy_from_original(&src, &dst, &patterns).await.unwrap();
        assert!(again.copied.is_empty());
        assert_eq!(again.skipped_existing.len(), 5);
    }

    fn assert_tree_copied(src: &Path, dst: &Path) {
        let link = dst.join("node_modules/.bin/pkg");
        assert!(link.is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../pkg/cli.sh")
        );
        assert_eq!(fs::read_to_string(&link).unwrap(), "#!/bin/sh\n");
        assert_eq!(mode(&dst.join("node_modules/pkg/cli.sh")), 0o755);
        assert_eq!(
            fs::read(dst.join("node_modules/pkg/index.js")).unwrap(),
            fs::read(src.join("node_modules/pkg/index.js")).unwrap()
        );
    }

    #[test]
    fn fallback_copy_preserves_symlinks_and_permissions() {
        let tmp = TempDir::new().unwrap();
        let (src, dst) = (tmp.path().join("main"), tmp.path().join("wt"));
        source_tree(&src);
        fs::create_dir(&dst).unwrap();

        copy_recursive(&src.join("node_modules"), &dst.join("node_modules"), true).unwrap();
        assert_tree_copied(&src, &dst);

        copy_recursive(&src.join(".env"), &dst.join(".env"), true).unwrap();
        assert_eq!(mode(&dst.join(".env")), 0o600);

        copy_recursive(&src.join(".env.local"), &dst.join(".env.local"), true).unwrap();
        assert!(!dst.join(".env.local").is_symlink());

        let ro = src.join("readonly");
        write(&ro, "f", "x");
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        copy_recursive(&ro, &dst.join("readonly"), true).unwrap();
        assert_eq!(mode(&dst.join("readonly")), 0o555);
        assert_eq!(fs::read_to_string(dst.join("readonly/f")).unwrap(), "x");
        for dir in [&ro, &dst.join("readonly")] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_patterns_escaping_the_checkout() {
        let tmp = TempDir::new().unwrap();
        for pattern in ["../secrets", "/etc/hosts", "a/../../b", "", "[unclosed"] {
            let err = copy_from_original(tmp.path(), tmp.path(), &[pattern.to_string()])
                .await
                .unwrap_err();
            assert!(
                matches!(err, CopyError::InvalidPattern { .. }),
                "{pattern}: {err:?}"
            );
        }
    }
}
