use crate::cmd::git;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// e.g. `origin/main`.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<FileChange>,
}

impl StatusSummary {
    /// Uncommitted or untracked changes (ignored files don't count).
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Relative to the worktree root. For renames, the new path.
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
    Other,
}

pub async fn status(dir: &Path) -> Result<StatusSummary> {
    let cmd = git(dir)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ])
        // Don't take the index lock just to refresh stat info: the user may be
        // running git in this checkout at the same time.
        .env("GIT_OPTIONAL_LOCKS", "0");
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(cmd.failure(&out));
    }
    Ok(parse_status(&out.stdout))
}

/// Parses `git status --porcelain=v2 --branch -z`.
fn parse_status(bytes: &[u8]) -> StatusSummary {
    let mut summary = StatusSummary::default();
    let mut entries = bytes
        .split(|b| *b == 0)
        .map(|e| String::from_utf8_lossy(e).into_owned());
    while let Some(entry) = entries.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" if value != "(detached)" => summary.branch = Some(value.into()),
                "branch.upstream" => summary.upstream = Some(value.into()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            summary.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            summary.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        // Fields are space-separated; the path is last and may contain spaces.
        let change = match entry.as_bytes().first() {
            Some(b'1') => entry.splitn(9, ' ').nth(8).map(|path| FileChange {
                path: path.to_string(),
                kind: kind_from_xy(entry.get(2..4).unwrap_or("")),
            }),
            Some(b'2') => {
                // The original path follows as its own NUL-terminated entry.
                entries.next();
                entry.splitn(10, ' ').nth(9).map(|path| FileChange {
                    path: path.to_string(),
                    kind: ChangeKind::Renamed,
                })
            }
            Some(b'u') => entry.splitn(11, ' ').nth(10).map(|path| FileChange {
                path: path.to_string(),
                kind: ChangeKind::Conflicted,
            }),
            Some(b'?') => entry.get(2..).map(|path| FileChange {
                path: path.to_string(),
                kind: ChangeKind::Untracked,
            }),
            _ => None,
        };
        summary.changes.extend(change);
    }
    summary
}

/// `XY` is the index and worktree state of an ordinary changed entry.
fn kind_from_xy(xy: &str) -> ChangeKind {
    if xy.contains('D') {
        ChangeKind::Deleted
    } else if xy.contains('A') {
        ChangeKind::Added
    } else if xy.contains(['M', 'T']) {
        ChangeKind::Modified
    } else {
        ChangeKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch;
    use crate::testutil::{Remote, commit_file, git as git_sync};

    fn change(path: &str, kind: ChangeKind) -> FileChange {
        FileChange {
            path: path.into(),
            kind,
        }
    }

    #[test]
    fn parses_porcelain_v2() {
        let raw = "# branch.oid 5af236f6572310e8a64dc153b85f06ad523b42a8\0\
            # branch.head feat/x\0\
            # branch.upstream origin/feat/x\0\
            # branch.ab +2 -3\0\
            1 .M N... 100644 100644 100644 aaa aaa src/main.rs\0\
            1 A. N... 000000 100644 100644 000 bbb new file.txt\0\
            1 D. N... 100644 000000 000000 ccc 000 gone.txt\0\
            1 .T N... 100644 120000 120000 ddd ddd link\0\
            2 RM N... 100644 100644 100644 eee eee R100 new name.rs\0old name.rs\0\
            u UU N... 100644 100644 100644 100644 f1 f2 f3 conflict.txt\0\
            ? untracked dir/\0\
            ! ignored.log\0";
        let s = parse_status(raw.as_bytes());
        assert_eq!(s.branch.as_deref(), Some("feat/x"));
        assert_eq!(s.upstream.as_deref(), Some("origin/feat/x"));
        assert_eq!((s.ahead, s.behind), (2, 3));
        assert_eq!(
            s.changes,
            vec![
                change("src/main.rs", ChangeKind::Modified),
                change("new file.txt", ChangeKind::Added),
                change("gone.txt", ChangeKind::Deleted),
                change("link", ChangeKind::Modified),
                change("new name.rs", ChangeKind::Renamed),
                change("conflict.txt", ChangeKind::Conflicted),
                change("untracked dir/", ChangeKind::Untracked),
            ]
        );
        assert!(s.is_dirty());
    }

    #[test]
    fn parses_detached_and_clean() {
        let s = parse_status(b"# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(s, StatusSummary::default());
        assert!(!s.is_dirty());
    }

    #[tokio::test]
    async fn real_status() {
        let r = Remote::new().await;
        let s = status(&r.clone).await.unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert!(!s.is_dirty());

        commit_file(&r.clone, "local.txt", "x\n", "local");
        r.push_upstream_commit("main", "remote.txt");
        fetch(&r.clone, "origin", &[]).await.unwrap();
        std::fs::write(r.clone.join("README.md"), "changed\n").unwrap();
        std::fs::write(r.clone.join("new.txt"), "new\n").unwrap();
        std::fs::write(r.clone.join("staged.txt"), "s\n").unwrap();
        git_sync(&r.clone, &["add", "staged.txt"]);
        git_sync(&r.clone, &["rm", "-q", "local.txt"]);

        let s = status(&r.clone).await.unwrap();
        assert_eq!((s.ahead, s.behind), (1, 1));
        assert_eq!(
            s.changes,
            vec![
                change("README.md", ChangeKind::Modified),
                change("local.txt", ChangeKind::Deleted),
                change("staged.txt", ChangeKind::Added),
                change("new.txt", ChangeKind::Untracked),
            ]
        );
    }
}
