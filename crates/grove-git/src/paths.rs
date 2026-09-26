use std::path::{Component, Path, PathBuf};

/// Resolves symlinks (macOS `/var` → `/private/var`, `/tmp` → `/private/tmp`)
/// so paths from different sources compare equal. Works for paths that don't
/// exist: the longest existing ancestor is resolved and the rest appended.
pub fn normalize_path(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(resolved) = std::fs::canonicalize(&path) {
        return resolved;
    }
    let path = lexically_clean(&path);
    let mut tail = Vec::new();
    let mut current = path.as_path();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(current) {
            return tail.iter().rev().fold(resolved, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                current = parent;
            }
            _ => return path,
        }
    }
}

/// Whether two paths refer to the same location, see [`normalize_path`].
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || normalize_path(a) == normalize_path(b)
}

/// Drops `.` components and applies `..` textually. Only used for paths that
/// don't exist, which `canonicalize` can't handle.
fn lexically_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolves_symlinked_ancestors() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(same_path(&link, &real));
        assert!(same_path(
            &link.join("missing/child"),
            &real.join("missing/child/")
        ));
        assert!(same_path(&real.join("a/../b"), &real.join("./b")));
        assert!(!same_path(&real.join("a"), &real.join("b")));
        assert_eq!(
            normalize_path(&link.join("x")),
            normalize_path(&real).join("x")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_var() {
        assert!(same_path(
            Path::new("/var/folders/nope"),
            Path::new("/private/var/folders/nope")
        ));
    }
}
