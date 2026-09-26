use std::path::PathBuf;

pub type Result<T, E = GitError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    NotInstalled,
    /// git ran and exited unsuccessfully.
    #[error("{}", format_command_error(command, stderr, *code))]
    Command {
        command: String,
        stderr: String,
        code: Option<i32>,
    },
    #[error("branch `{branch}` is already checked out at {}", path.display())]
    BranchCheckedOut { branch: String, path: PathBuf },
    #[error("branch `{0}` exists neither locally nor on origin")]
    BranchNotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn format_command_error(command: &str, stderr: &str, code: Option<i32>) -> String {
    let stderr = stderr.trim();
    match (stderr.is_empty(), code) {
        (false, _) => format!("`{command}` failed: {stderr}"),
        (true, Some(code)) => format!("`{command}` failed with exit code {code}"),
        (true, None) => format!("`{command}` was killed by a signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_display() {
        let e = GitError::Command {
            command: "git -C /r pull --ff-only".into(),
            stderr: "fatal: Not possible to fast-forward, aborting.\n".into(),
            code: Some(128),
        };
        assert_eq!(
            e.to_string(),
            "`git -C /r pull --ff-only` failed: fatal: Not possible to fast-forward, aborting."
        );
        let e = GitError::Command {
            command: "git status".into(),
            stderr: " \n".into(),
            code: Some(1),
        };
        assert_eq!(e.to_string(), "`git status` failed with exit code 1");
    }
}
