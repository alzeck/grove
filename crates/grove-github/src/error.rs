#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("the GitHub CLI (`gh`) is not installed or not on PATH")]
    NotInstalled,
    #[error("the GitHub CLI is not logged in (run `gh auth login`): {0}")]
    NotAuthenticated(String),
    #[error("`gh {}` failed: {}", args.join(" "), stderr.trim())]
    Command { args: Vec<String>, stderr: String },
    #[error("unexpected output from gh: {0}")]
    Parse(String),
}
