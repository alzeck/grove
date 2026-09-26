use crate::cmd::git;
use crate::error::{GitError, Result};
use std::path::Path;

pub async fn clone(url: &str, dest: &Path) -> Result<()> {
    let dest = std::path::absolute(dest)?;
    let parent = dest.parent().unwrap_or(Path::new("/"));
    tokio::fs::create_dir_all(parent).await?;
    git(parent)
        .args(["clone", "--", url])
        .arg(&dest)
        .run()
        .await?;
    Ok(())
}

/// The branch `origin/HEAD` points at; otherwise `main` or `master` if either
/// exists locally or on origin; otherwise `main`.
pub async fn default_branch(repo: &Path) -> Result<String> {
    let out = git(repo)
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .await?;
    if out.status.success() {
        let name = String::from_utf8_lossy(&out.stdout);
        let name = name.trim();
        if let Some(branch) = name.strip_prefix("origin/").filter(|b| !b.is_empty()) {
            return Ok(branch.to_string());
        }
    }
    for candidate in ["main", "master"] {
        if branch_exists_local(repo, candidate).await?
            || branch_exists_remote(repo, "origin", candidate).await?
        {
            return Ok(candidate.to_string());
        }
    }
    Ok("main".to_string())
}

/// `None` when HEAD is detached.
pub async fn current_branch(dir: &Path) -> Result<Option<String>> {
    let out = git(dir).args(["branch", "--show-current"]).run().await?;
    let name = out.trim();
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// Short sha of HEAD.
pub async fn head_commit(dir: &Path) -> Result<String> {
    let out = git(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .run()
        .await?;
    Ok(out.trim().to_string())
}

pub async fn branch_exists_local(repo: &Path, branch: &str) -> Result<bool> {
    ref_exists(repo, &format!("refs/heads/{branch}")).await
}

/// Whether the remote-tracking ref `refs/remotes/<remote>/<branch>` exists.
/// Only looks at local refs; fetch first for an up-to-date answer.
pub async fn branch_exists_remote(repo: &Path, remote: &str, branch: &str) -> Result<bool> {
    ref_exists(repo, &format!("refs/remotes/{remote}/{branch}")).await
}

async fn ref_exists(repo: &Path, full_ref: &str) -> Result<bool> {
    git(repo)
        .args(["show-ref", "--verify", "--quiet", full_ref])
        .probe()
        .await
}

/// Asks the remote itself (network) whether it has `branch`.
pub async fn remote_branch_exists_ls(repo: &Path, remote: &str, branch: &str) -> Result<bool> {
    let wanted = format!("refs/heads/{branch}");
    let out = git(repo)
        .args(["ls-remote", "--heads", remote, &wanted])
        .run()
        .await?;
    // ls-remote patterns match on the tail of a ref, so check for an exact hit.
    Ok(out
        .lines()
        .any(|line| line.split('\t').nth(1) == Some(wanted.as_str())))
}

pub async fn fetch(repo: &Path, remote: &str, refspecs: &[String]) -> Result<()> {
    git(repo)
        .args(["fetch", "--quiet", remote])
        .args(refspecs)
        .run()
        .await?;
    Ok(())
}

/// `git pull --ff-only`. Returns git's combined stdout and stderr. A diverged
/// branch is a [`GitError::Command`] carrying git's explanation.
pub async fn pull_ff_only(dir: &Path) -> Result<String> {
    // --no-rebase: a `pull.rebase` setting must not turn this into a rebase.
    let cmd = git(dir).args(["pull", "--ff-only", "--no-rebase"]);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(cmd.failure(&out));
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// Makes `branch` track `upstream` (e.g. `origin/feature`).
pub async fn set_upstream(dir: &Path, branch: &str, upstream: &str) -> Result<()> {
    git(dir)
        .args(["branch", "--set-upstream-to", upstream, branch])
        .run()
        .await?;
    Ok(())
}

impl GitError {
    /// Whether this is git refusing a fast-forward-only pull because the
    /// branch has diverged from its upstream.
    pub fn is_diverged(&self) -> bool {
        matches!(self, GitError::Command { stderr, .. }
            if stderr.contains("Not possible to fast-forward")
                || stderr.contains("Diverging branches"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Remote, commit_file, git as git_sync, init_repo};
    use tempfile::TempDir;

    #[tokio::test]
    async fn default_branch_from_origin_head() {
        let r = Remote::new().await;
        assert_eq!(default_branch(&r.clone).await.unwrap(), "main");

        // origin/HEAD pointing elsewhere wins.
        git_sync(
            &r.clone,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/feature",
            ],
        );
        assert_eq!(default_branch(&r.clone).await.unwrap(), "feature");
    }

    #[tokio::test]
    async fn default_branch_fallbacks() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        assert_eq!(default_branch(tmp.path()).await.unwrap(), "main");

        git_sync(tmp.path(), &["branch", "-m", "main", "master"]);
        assert_eq!(default_branch(tmp.path()).await.unwrap(), "master");

        git_sync(tmp.path(), &["branch", "-m", "master", "trunk"]);
        assert_eq!(default_branch(tmp.path()).await.unwrap(), "main");
    }

    #[tokio::test]
    async fn branches_and_head() {
        let r = Remote::new().await;
        assert_eq!(
            current_branch(&r.clone).await.unwrap().as_deref(),
            Some("main")
        );
        let sha = head_commit(&r.clone).await.unwrap();
        assert!(sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()));

        assert!(branch_exists_local(&r.clone, "main").await.unwrap());
        assert!(!branch_exists_local(&r.clone, "feature").await.unwrap());
        assert!(
            branch_exists_remote(&r.clone, "origin", "feature")
                .await
                .unwrap()
        );
        assert!(
            !branch_exists_remote(&r.clone, "origin", "nope")
                .await
                .unwrap()
        );
        assert!(
            remote_branch_exists_ls(&r.clone, "origin", "feature")
                .await
                .unwrap()
        );
        assert!(
            !remote_branch_exists_ls(&r.clone, "origin", "feat")
                .await
                .unwrap()
        );

        git_sync(&r.clone, &["checkout", "-q", "--detach"]);
        assert_eq!(current_branch(&r.clone).await.unwrap(), None);
    }

    #[tokio::test]
    async fn errors_outside_a_repo() {
        let tmp = TempDir::new().unwrap();
        let err = branch_exists_local(tmp.path(), "main").await.unwrap_err();
        assert!(
            matches!(
                &err,
                GitError::Command {
                    code: Some(128),
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[tokio::test]
    async fn fetch_new_branch() {
        let r = Remote::new().await;
        let seed = r.tmp.path().join("seed");
        git_sync(&seed, &["branch", "late"]);
        git_sync(&seed, &["push", "-q", r.origin.to_str().unwrap(), "late"]);
        assert!(
            !branch_exists_remote(&r.clone, "origin", "late")
                .await
                .unwrap()
        );

        fetch(
            &r.clone,
            "origin",
            &["refs/heads/late:refs/remotes/origin/late".to_string()],
        )
        .await
        .unwrap();
        assert!(
            branch_exists_remote(&r.clone, "origin", "late")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn pull_fast_forward_and_divergence() {
        let r = Remote::new().await;
        r.push_upstream_commit("main", "up1.txt");
        let out = pull_ff_only(&r.clone).await.unwrap();
        assert!(out.contains("Fast-forward"), "{out}");
        assert!(r.clone.join("up1.txt").exists());

        r.push_upstream_commit("main", "up2.txt");
        commit_file(&r.clone, "local.txt", "local\n", "local change");
        let err = pull_ff_only(&r.clone).await.unwrap_err();
        assert!(err.is_diverged(), "{err}");
        assert!(!r.clone.join("up2.txt").exists());
    }
}
