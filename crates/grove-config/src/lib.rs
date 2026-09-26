//! Grove configuration: the user config (`~/.grove/config.toml`), the
//! workspace (`grove.toml` + `projects/*.toml`), merging of personal
//! overrides, validation, and templating.

mod duration;
mod error;
mod load;
mod paths;
mod project;
mod template;
mod user;
mod validate;
mod workspace;

pub use error::{ConfigError, Diagnostic};
pub use load::{Config, LoadedWorkspace, Project, load_workspace};
pub use paths::{GroveHome, expand_tilde};
pub use project::{
    DatabaseConfig, DbMode, DomainConfig, FreshStrategy, PortConfig, ProcessConfig, ProcessRef,
    ProjectConfig, RESERVED_PROCESS_NAMES, ReadyCheck, ReadyKind,
};
pub use template::{TemplateError, Templates};
pub use user::{Appearance, UserConfig, WorkspaceSource};
pub use workspace::{DomainTemplates, PostgresConfig, WorkspaceConfig};

use std::time::Duration;

pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PORT_RANGE: (u16, u16) = (4100, 4999);
pub const DEFAULT_POSTGRES_URL: &str = "postgres://localhost:5432";
pub const DEFAULT_CLUSTER: &str = "default";

/// Cluster names are DNS labels: lowercase ASCII letters, digits and dashes,
/// starting with a letter or digit, at most 30 characters.
pub fn is_valid_cluster_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 30
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Turns arbitrary text (a branch name, a PR title) into a valid cluster name.
pub fn slugify_cluster_name(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in input.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let mut out: String = out.trim_matches('-').chars().take(30).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("cluster");
    }
    out
}

/// Project names: `[a-z][a-z0-9-]*`. They appear in domains and paths.
pub fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Process names: `[a-z][a-z0-9_-]*`, excluding reserved names.
pub fn is_valid_process_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !RESERVED_PROCESS_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify() {
        assert_eq!(slugify_cluster_name("feat/New Thing!"), "feat-new-thing");
        assert_eq!(slugify_cluster_name("--"), "cluster");
        assert_eq!(
            slugify_cluster_name("a-very-long-branch-name-that-goes-on-and-on"),
            "a-very-long-branch-name-that-g"
        );
        assert!(is_valid_cluster_name(&slugify_cluster_name("x/y/z")));
    }

    #[test]
    fn names() {
        assert!(is_valid_project_name("api"));
        assert!(is_valid_project_name("my-api2"));
        assert!(!is_valid_project_name("2api"));
        assert!(!is_valid_project_name("My"));
        assert!(is_valid_process_name("web_1"));
        assert!(!is_valid_process_name("db"));
        assert!(is_valid_cluster_name("pr-123"));
        assert!(!is_valid_cluster_name("-x"));
    }
}
