use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Grove isn't configured: {0}")]
    NotConfigured(String),
    #[error("no cluster named `{0}`")]
    UnknownCluster(String),
    #[error("no project named `{0}`")]
    UnknownProject(String),
    #[error("no process `{process}` in project `{project}`")]
    UnknownProcess { project: String, process: String },
    #[error("{0}")]
    Invalid(String),
    #[error("cluster `{0}` is busy with another operation")]
    Busy(String),
    #[error("main clone of `{project}` not found at {}", .path.display())]
    MissingClone { project: String, path: PathBuf },
    #[error("port {port} is in use{}", .owner.as_ref().map(|o| format!(" by {o}")).unwrap_or_default())]
    PortInUse { port: u16, owner: Option<String> },
    #[error("no free port in range {0}-{1}")]
    NoFreePort(u16, u16),
    #[error("{0}")]
    Template(String),
    #[error("worktrees have uncommitted changes: {}", .0.join(", "))]
    DirtyWorktrees(Vec<String>),
    #[error("`{command}` failed: {message}")]
    CommandFailed { command: String, message: String },
    #[error(transparent)]
    Git(#[from] grove_git::GitError),
    #[error(transparent)]
    GitCopy(#[from] grove_git::CopyError),
    #[error(transparent)]
    GitHub(#[from] grove_github::GhError),
    #[error(transparent)]
    Db(#[from] grove_db::DbError),
    #[error(transparent)]
    Proc(#[from] grove_proc::ProcError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<grove_config::TemplateError> for CoreError {
    fn from(e: grove_config::TemplateError) -> Self {
        CoreError::Template(e.to_string())
    }
}
