use crate::duration;
use crate::paths::expand_tilde;
use crate::workspace::PostgresConfig;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// `~/.grove/config.toml`: per-machine settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Local path or git URL of the workspace.
    pub workspace: Option<String>,
    /// Where main clones live by default (`<clones_root>/<project>`).
    pub clones_root: Option<String>,
    /// Workspace projects enabled on this machine. `None` enables all.
    pub projects: Option<Vec<String>>,
    /// Editor command template, e.g. `zed {{ path }}`.
    pub editor: Option<String>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub idle_timeout: Option<Duration>,
    pub port_range: Option<[u16; 2]>,
    #[serde(default)]
    pub postgres: PostgresConfig,
    /// Personal overrides, deep-merged over each project's config.
    #[serde(default)]
    pub overrides: IndexMap<String, toml::Table>,
    /// Light or dark UI.
    #[serde(default)]
    pub appearance: Appearance,
}

/// The app's light/dark appearance: `system` follows macOS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    /// The value as written in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSource {
    Path(PathBuf),
    Git(String),
}

impl WorkspaceSource {
    pub fn parse(s: &str) -> Self {
        if looks_like_git_url(s) {
            WorkspaceSource::Git(s.to_string())
        } else {
            WorkspaceSource::Path(expand_tilde(s))
        }
    }
}

impl UserConfig {
    pub fn workspace_source(&self) -> Option<WorkspaceSource> {
        self.workspace.as_deref().map(WorkspaceSource::parse)
    }

    pub fn clones_root(&self) -> PathBuf {
        expand_tilde(self.clones_root.as_deref().unwrap_or("~/Developer"))
    }

    pub fn is_enabled(&self, project: &str) -> bool {
        match &self.projects {
            None => true,
            Some(list) => list.iter().any(|p| p == project),
        }
    }
}

fn looks_like_git_url(s: &str) -> bool {
    if s.contains("://") {
        return true;
    }
    // scp-like syntax: user@host:path
    match (s.find('@'), s.find(':')) {
        (Some(at), Some(colon)) => at < colon && !s.starts_with('/') && !s.starts_with('~'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_source() {
        assert_eq!(
            WorkspaceSource::parse("git@github.com:acme/ws.git"),
            WorkspaceSource::Git("git@github.com:acme/ws.git".into())
        );
        assert_eq!(
            WorkspaceSource::parse("https://github.com/acme/ws"),
            WorkspaceSource::Git("https://github.com/acme/ws".into())
        );
        assert_eq!(
            WorkspaceSource::parse("/tmp/ws"),
            WorkspaceSource::Path("/tmp/ws".into())
        );
    }

    #[test]
    fn parses_user_config() {
        let cfg: UserConfig = toml::from_str(
            r#"
            workspace = "~/team/grove-workspace"
            clones_root = "~/Developer"
            projects = ["api"]
            idle_timeout = "0"
            [postgres]
            url = "postgres://me@localhost:5432"
            [overrides.api]
            path = "~/work/api"
            copy_from_original = [".env"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.idle_timeout, Some(Duration::ZERO));
        assert!(cfg.is_enabled("api"));
        assert!(!cfg.is_enabled("frontend"));
        assert!(cfg.overrides["api"].contains_key("path"));
        assert_eq!(cfg.appearance, Appearance::System);
    }

    #[test]
    fn parses_appearance() {
        for (text, expected) in [
            ("system", Appearance::System),
            ("light", Appearance::Light),
            ("dark", Appearance::Dark),
        ] {
            let cfg: UserConfig = toml::from_str(&format!("appearance = \"{text}\"")).unwrap();
            assert_eq!(cfg.appearance, expected);
            assert_eq!(expected.as_str(), text);
        }
        assert!(toml::from_str::<UserConfig>("appearance = \"sepia\"").is_err());
    }
}
