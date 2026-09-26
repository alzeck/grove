//! Git operations for Grove, done by shelling out to the `git` binary so the
//! user's config, credential helpers and hooks all apply: cloning, branches,
//! worktrees, status, pulling, and copying files from a main clone into a
//! worktree.

mod cmd;
mod copy;
mod error;
mod paths;
mod repo;
mod status;
mod worktree;

#[cfg(test)]
mod testutil;

pub use cmd::git_version;
pub use copy::{CopyError, CopyReport, copy_from_original};
pub use error::{GitError, Result};
pub use paths::{normalize_path, same_path};
pub use repo::{
    branch_exists_local, branch_exists_remote, clone, current_branch, default_branch, fetch,
    head_commit, pull_ff_only, remote_branch_exists_ls, set_upstream,
};
pub use status::{ChangeKind, FileChange, StatusSummary, status};
pub use worktree::{
    WorktreeBranch, WorktreeInfo, branch_checked_out_at, worktree_add, worktree_list,
    worktree_remove,
};
