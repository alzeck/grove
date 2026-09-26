use std::path::{Path, PathBuf};

/// Everything Grove owns lives under one directory, `~/.grove` by default.
/// `GROVE_HOME` overrides it (used by tests and for running a second copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroveHome {
    root: PathBuf,
}

impl GroveHome {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn from_env() -> Self {
        match std::env::var_os("GROVE_HOME") {
            Some(p) if !p.is_empty() => Self::new(p),
            _ => Self::new(home_dir().join(".grove")),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// Where a workspace given by git URL is cloned.
    pub fn workspace_clone_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    pub fn worktree_path(&self, cluster: &str, project: &str) -> PathBuf {
        self.worktrees_dir().join(cluster).join(project)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn cluster_logs_dir(&self, cluster: &str) -> PathBuf {
        self.logs_dir().join(cluster)
    }

    pub fn log_path(&self, cluster: &str, project: &str, process: &str) -> PathBuf {
        self.cluster_logs_dir(cluster)
            .join(project)
            .join(format!("{process}.log"))
    }

    pub fn ca_dir(&self) -> PathBuf {
        self.root.join("ca")
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.root.join("grove.sock")
    }
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` or `~/`.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let h = GroveHome::new("/g");
        assert_eq!(
            h.worktree_path("pr-1", "api"),
            PathBuf::from("/g/worktrees/pr-1/api")
        );
        assert_eq!(
            h.log_path("c", "p", "web"),
            PathBuf::from("/g/logs/c/p/web.log")
        );
        assert_eq!(expand_tilde("~/x"), home_dir().join("x"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
