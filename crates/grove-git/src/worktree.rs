use crate::cmd::git;
use crate::error::{GitError, Result};
use crate::paths::normalize_path;
use crate::repo::{branch_exists_local, branch_exists_remote};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Canonicalized with [`normalize_path`](crate::normalize_path).
    pub path: PathBuf,
    /// Full sha of HEAD; `None` for a bare repo or an unborn branch.
    pub head: Option<String>,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    /// The directory is gone; `git worktree prune` would forget it.
    pub prunable: bool,
    /// The main working tree (the original clone). Always listed first.
    pub is_main: bool,
}

/// What to check out in a new worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeBranch {
    /// A local branch. If only `origin/<name>` exists, a local branch
    /// tracking it is created.
    Existing(String),
    /// A new branch `name` starting at `base`.
    New { name: String, base: String },
    /// A detached HEAD at a commit-ish.
    Detached(String),
}

pub async fn worktree_list(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let cmd = git(repo).args(["worktree", "list", "--porcelain", "-z"]);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(cmd.failure(&out));
    }
    let mut list = parse_worktree_list(&out.stdout);
    for wt in &mut list {
        wt.path = normalize_path(&wt.path);
    }
    Ok(list)
}

/// Where `branch` is checked out (main clone or linked worktree), if anywhere.
/// Worktrees whose directory no longer exists are ignored: [`worktree_add`]
/// prunes them first.
pub async fn branch_checked_out_at(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(worktree_list(repo)
        .await?
        .into_iter()
        .find(|wt| !wt.prunable && wt.branch.as_deref() == Some(branch))
        .map(|wt| wt.path))
}

/// Creates a worktree of `repo` at `path` (and any missing parent dirs).
/// Stale worktree entries whose directories were deleted are pruned first,
/// so their paths and branches can be reused.
pub async fn worktree_add(repo: &Path, path: &Path, branch: &WorktreeBranch) -> Result<()> {
    prune(repo).await?;
    let path = std::path::absolute(path)?;
    let add = git(repo).args(["worktree", "add"]);
    let add = match branch {
        WorktreeBranch::Existing(name) => {
            if let Some(at) = branch_checked_out_at(repo, name).await? {
                return Err(GitError::BranchCheckedOut {
                    branch: name.clone(),
                    path: at,
                });
            }
            if branch_exists_local(repo, name).await? {
                add.arg(&path).arg(name)
            } else if branch_exists_remote(repo, "origin", name).await? {
                add.args(["--track", "-b", name])
                    .arg(&path)
                    .arg(format!("origin/{name}"))
            } else {
                return Err(GitError::BranchNotFound(name.clone()));
            }
        }
        WorktreeBranch::New { name, base } => add.args(["-b", name]).arg(&path).arg(base),
        WorktreeBranch::Detached(commit) => add.arg("--detach").arg(&path).arg(commit),
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    add.run().await?;
    Ok(())
}

/// Removes a worktree; `force` is needed when it has uncommitted or untracked
/// changes. A worktree whose directory is already gone is just pruned.
pub async fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<()> {
    if tokio::fs::try_exists(path).await? {
        let mut remove = git(repo).args(["worktree", "remove"]);
        if force {
            remove = remove.arg("--force");
        }
        remove.arg(path).run().await?;
    }
    prune(repo).await
}

async fn prune(repo: &Path) -> Result<()> {
    git(repo).args(["worktree", "prune"]).run().await?;
    Ok(())
}

/// Parses `git worktree list --porcelain -z`: NUL-terminated `key value`
/// lines, with an empty line between worktrees.
fn parse_worktree_list(bytes: &[u8]) -> Vec<WorktreeInfo> {
    let mut list = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in bytes.split(|b| *b == 0) {
        if line.is_empty() {
            list.extend(current.take());
            continue;
        }
        let (key, value) = match line.iter().position(|b| *b == b' ') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, &[][..]),
        };
        if key == b"worktree" {
            list.extend(current.take());
            current = Some(WorktreeInfo {
                path: PathBuf::from(OsStr::from_bytes(value)),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: false,
                prunable: false,
                is_main: list.is_empty(),
            });
            continue;
        }
        let Some(wt) = current.as_mut() else { continue };
        let value = String::from_utf8_lossy(value);
        match key {
            // An unborn branch shows the null sha.
            b"HEAD" if !value.bytes().all(|b| b == b'0') => wt.head = Some(value.into_owned()),
            b"branch" => {
                let short = value.strip_prefix("refs/heads/").unwrap_or(&value);
                wt.branch = Some(short.to_string());
            }
            b"detached" => wt.detached = true,
            b"bare" => wt.bare = true,
            b"locked" => wt.locked = true,
            b"prunable" => wt.prunable = true,
            _ => {}
        }
    }
    list.extend(current);
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Remote, git as git_sync};
    use crate::{current_branch, same_path, status};

    const SHA: &str = "5af236f6572310e8a64dc153b85f06ad523b42a8";

    #[test]
    fn parses_porcelain() {
        let raw = format!(
            "worktree /repos/api\0HEAD {SHA}\0branch refs/heads/main\0\0\
             worktree /wt/feat\0HEAD {SHA}\0branch refs/heads/feat/x\0locked reason here\0\0\
             worktree /wt/detached one\0HEAD {SHA}\0detached\0prunable gitdir file points to non-existent location\0\0\
             worktree /wt/orphan\0HEAD 0000000000000000000000000000000000000000\0branch refs/heads/orphan\0\0"
        );
        let list = parse_worktree_list(raw.as_bytes());
        assert_eq!(list.len(), 4);
        assert_eq!(
            list[0],
            WorktreeInfo {
                path: "/repos/api".into(),
                head: Some(SHA.into()),
                branch: Some("main".into()),
                detached: false,
                bare: false,
                locked: false,
                prunable: false,
                is_main: true,
            }
        );
        assert_eq!(list[1].branch.as_deref(), Some("feat/x"));
        assert!(list[1].locked && !list[1].is_main);
        assert_eq!(list[2].path, PathBuf::from("/wt/detached one"));
        assert!(list[2].detached && list[2].prunable && list[2].branch.is_none());
        assert_eq!(list[3].head, None);

        let bare = parse_worktree_list(b"worktree /repos/api.git\0bare\0\0");
        assert!(bare[0].bare && bare[0].is_main && bare[0].head.is_none());
        assert!(parse_worktree_list(b"").is_empty());
    }

    #[tokio::test]
    async fn add_new_existing_and_remote_only_branches() {
        let r = Remote::new().await;
        let wts = r.tmp.path().join("worktrees");

        let new = wts.join("c1/api");
        let branch = WorktreeBranch::New {
            name: "feat/new".into(),
            base: "main".into(),
        };
        worktree_add(&r.clone, &new, &branch).await.unwrap();
        assert_eq!(
            current_branch(&new).await.unwrap().as_deref(),
            Some("feat/new")
        );

        git_sync(&r.clone, &["branch", "local-only"]);
        let local = wts.join("c2/api");
        worktree_add(
            &r.clone,
            &local,
            &WorktreeBranch::Existing("local-only".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            current_branch(&local).await.unwrap().as_deref(),
            Some("local-only")
        );

        // `feature` only exists as origin/feature: a tracking branch is made.
        let remote = wts.join("c3/api");
        worktree_add(
            &r.clone,
            &remote,
            &WorktreeBranch::Existing("feature".into()),
        )
        .await
        .unwrap();
        let st = status(&remote).await.unwrap();
        assert_eq!(st.branch.as_deref(), Some("feature"));
        assert_eq!(st.upstream.as_deref(), Some("origin/feature"));
    }

    #[tokio::test]
    async fn refuses_checked_out_and_missing_branches() {
        let r = Remote::new().await;
        let wt = r.tmp.path().join("wt");

        let err = worktree_add(&r.clone, &wt, &WorktreeBranch::Existing("main".into()))
            .await
            .unwrap_err();
        match err {
            GitError::BranchCheckedOut { branch, path } => {
                assert_eq!(branch, "main");
                assert!(same_path(&path, &r.clone));
            }
            other => panic!("unexpected {other:?}"),
        }

        worktree_add(&r.clone, &wt, &WorktreeBranch::Existing("feature".into()))
            .await
            .unwrap();
        let again = r.tmp.path().join("wt2");
        let err = worktree_add(
            &r.clone,
            &again,
            &WorktreeBranch::Existing("feature".into()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, GitError::BranchCheckedOut { path, .. } if same_path(path, &wt)),
            "{err:?}"
        );

        let err = worktree_add(&r.clone, &again, &WorktreeBranch::Existing("nope".into()))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, GitError::BranchNotFound(b) if b == "nope"),
            "{err:?}"
        );
        assert!(!again.exists());
    }

    #[tokio::test]
    async fn lists_worktrees_including_detached() {
        let r = Remote::new().await;
        let feat = r.tmp.path().join("feat");
        let detached = r.tmp.path().join("detached");
        worktree_add(&r.clone, &feat, &WorktreeBranch::Existing("feature".into()))
            .await
            .unwrap();
        worktree_add(
            &r.clone,
            &detached,
            &WorktreeBranch::Detached("HEAD".into()),
        )
        .await
        .unwrap();

        let list = worktree_list(&r.clone).await.unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_main);
        assert_eq!(list[0].path, normalize_path(&r.clone));
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        // Linked worktrees are listed in git's internal order (by name).
        let find = |p: &Path| list.iter().find(|w| w.path == normalize_path(p)).unwrap();
        assert_eq!(find(&feat).branch.as_deref(), Some("feature"));
        let det = find(&detached);
        assert!(det.detached && det.branch.is_none() && !det.is_main);
        assert_eq!(det.head.as_deref().unwrap().len(), 40);
        assert!(!list.iter().any(|w| w.bare || w.locked || w.prunable));

        assert_eq!(
            branch_checked_out_at(&r.clone, "feature").await.unwrap(),
            Some(normalize_path(&feat))
        );
        assert_eq!(
            branch_checked_out_at(&r.clone, "other").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn remove_dirty_needs_force() {
        let r = Remote::new().await;
        let wt = r.tmp.path().join("wt");
        worktree_add(&r.clone, &wt, &WorktreeBranch::Existing("feature".into()))
            .await
            .unwrap();
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

        let err = worktree_remove(&r.clone, &wt, false).await.unwrap_err();
        assert!(matches!(err, GitError::Command { .. }), "{err:?}");
        assert!(wt.exists());

        worktree_remove(&r.clone, &wt, true).await.unwrap();
        assert!(!wt.exists());
        assert_eq!(worktree_list(&r.clone).await.unwrap().len(), 1);
        // The branch is kept.
        assert!(branch_exists_local(&r.clone, "feature").await.unwrap());
    }

    #[tokio::test]
    async fn deleted_worktree_dirs_are_pruned() {
        let r = Remote::new().await;
        let wt = r.tmp.path().join("wt");
        let branch = WorktreeBranch::Existing("feature".into());
        worktree_add(&r.clone, &wt, &branch).await.unwrap();
        std::fs::remove_dir_all(&wt).unwrap();

        assert!(worktree_list(&r.clone).await.unwrap()[1].prunable);
        assert_eq!(
            branch_checked_out_at(&r.clone, "feature").await.unwrap(),
            None
        );
        worktree_add(&r.clone, &wt, &branch).await.unwrap();
        assert!(wt.join("README.md").exists());

        std::fs::remove_dir_all(&wt).unwrap();
        worktree_remove(&r.clone, &wt, false).await.unwrap();
        assert_eq!(worktree_list(&r.clone).await.unwrap().len(), 1);
    }
}
