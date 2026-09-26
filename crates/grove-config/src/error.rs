use std::fmt;
use std::path::PathBuf;

/// One problem found while loading or validating config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: Option<PathBuf>,
    pub message: String,
}

impl Diagnostic {
    pub fn new(file: Option<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            file,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{}: {}", file.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Grove is not configured yet ({0} does not exist)")]
    NotConfigured(PathBuf),
    #[error("no workspace set in {0}")]
    NoWorkspace(PathBuf),
    #[error("workspace {0} has not been cloned yet")]
    WorkspaceMissing(PathBuf),
    #[error("{}", format_diagnostics(.0))]
    Invalid(Vec<Diagnostic>),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ConfigError {
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        match self {
            ConfigError::Invalid(d) => d.clone(),
            other => vec![Diagnostic::new(None, other.to_string())],
        }
    }
}

fn format_diagnostics(diags: &[Diagnostic]) -> String {
    let mut out = format!(
        "{} config problem{}:",
        diags.len(),
        if diags.len() == 1 { "" } else { "s" }
    );
    for d in diags {
        out.push_str("\n  - ");
        out.push_str(&d.to_string().replace('\n', "\n    "));
    }
    out
}
