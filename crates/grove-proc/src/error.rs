use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("working directory {0} is not a directory")]
    Cwd(PathBuf),
    #[error("could not set up a terminal: {0}")]
    Pty(String),
    #[error("could not start `{command}`: {message}")]
    Spawn { command: String, message: String },
    #[error("log file {path}: {source}")]
    Log {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("capturing the environment of {shell} in {dir}: {message}")]
    ShellEnv {
        shell: PathBuf,
        dir: PathBuf,
        message: String,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}
