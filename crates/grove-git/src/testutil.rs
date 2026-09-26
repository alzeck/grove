//! Throwaway repositories for tests. Git runs with no global or system
//! config, so the user's setup never leaks in (or gets touched).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Runs git synchronously in `dir`, panicking on failure; returns stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub fn commit_file(dir: &Path, file: &str, content: &str, message: &str) {
    let path = dir.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    git(dir, &["add", "--", file]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// A fresh repo on `main` with one commit.
pub fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    commit_file(dir, "README.md", "hello\n", "initial");
}

/// A bare `origin.git` (seeded with one commit on `main` and a `feature`
/// branch) and a `clone` of it made with [`crate::clone`].
pub struct Remote {
    pub tmp: TempDir,
    pub origin: PathBuf,
    pub clone: PathBuf,
}

impl Remote {
    pub async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let seed = tmp.path().join("seed");
        init_repo(&seed);
        git(&seed, &["branch", "feature"]);
        let origin = tmp.path().join("origin.git");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                "origin.git",
            ],
        );
        let clone = tmp.path().join("clone");
        crate::clone(origin.to_str().unwrap(), &clone)
            .await
            .unwrap();
        Self { tmp, origin, clone }
    }

    /// Makes a commit in another clone and pushes it to origin's `branch`.
    pub fn push_upstream_commit(&self, branch: &str, file: &str) {
        let other = self.tmp.path().join("other");
        if !other.exists() {
            git(
                self.tmp.path(),
                &["clone", "-q", self.origin.to_str().unwrap(), "other"],
            );
        }
        git(&other, &["checkout", "-q", branch]);
        git(&other, &["pull", "-q", "--ff-only"]);
        commit_file(&other, file, "upstream\n", "upstream change");
        git(&other, &["push", "-q", "origin", branch]);
    }
}
