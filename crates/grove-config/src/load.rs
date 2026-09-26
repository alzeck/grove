use crate::error::{ConfigError, Diagnostic};
use crate::paths::{GroveHome, expand_tilde};
use crate::project::ProjectConfig;
use crate::user::{UserConfig, WorkspaceSource};
use crate::validate::validate;
use crate::workspace::{DomainTemplates, WorkspaceConfig};
use crate::{DEFAULT_IDLE_TIMEOUT, DEFAULT_PORT_RANGE, DEFAULT_POSTGRES_URL};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A workspace directory: `grove.toml` plus every project in `projects/`.
#[derive(Debug, Clone)]
pub struct LoadedWorkspace {
    pub dir: PathBuf,
    pub config: WorkspaceConfig,
    /// All projects in the workspace, keyed by name, with the file each came from.
    pub projects: IndexMap<String, (ProjectConfig, PathBuf)>,
    pub warnings: Vec<Diagnostic>,
}

/// A fully loaded configuration: user config + workspace, restricted to the
/// projects enabled on this machine.
#[derive(Debug, Clone)]
pub struct Config {
    pub home: GroveHome,
    pub user: UserConfig,
    pub workspace: LoadedWorkspace,
    pub projects: IndexMap<String, Project>,
    pub warnings: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub config: ProjectConfig,
    /// The main clone on disk (may not exist yet).
    pub main_clone: PathBuf,
    /// The workspace file this project came from.
    pub file: PathBuf,
}

impl Project {
    pub fn name(&self) -> &str {
        &self.config.name
    }
}

impl Config {
    pub fn load(home: &GroveHome) -> Result<Self, ConfigError> {
        let user = Self::load_user(home)?;
        let workspace_dir = Self::workspace_dir(home, &user)?;
        Self::load_with(home.clone(), user, &workspace_dir)
    }

    /// Loads using an explicit workspace directory (tests, `grove validate`).
    pub fn load_with(
        home: GroveHome,
        user: UserConfig,
        workspace_dir: &Path,
    ) -> Result<Self, ConfigError> {
        let workspace = load_workspace(workspace_dir, &user.overrides)?;
        let mut warnings = workspace.warnings.clone();

        if let Some(enabled) = &user.projects {
            for name in enabled {
                if !workspace.projects.contains_key(name) {
                    warnings.push(Diagnostic::new(
                        Some(home.config_file()),
                        format!("project `{name}` is enabled but not in the workspace"),
                    ));
                }
            }
        }

        let clones_root = user.clones_root();
        let projects = workspace
            .projects
            .iter()
            .filter(|(name, _)| user.is_enabled(name))
            .map(|(name, (config, file))| {
                let main_clone = match &config.path {
                    Some(p) => expand_tilde(p),
                    None => clones_root.join(name),
                };
                (
                    name.clone(),
                    Project {
                        config: config.clone(),
                        main_clone,
                        file: file.clone(),
                    },
                )
            })
            .collect();

        Ok(Config {
            home,
            user,
            workspace,
            projects,
            warnings,
        })
    }

    pub fn load_user(home: &GroveHome) -> Result<UserConfig, ConfigError> {
        let path = home.config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::NotConfigured(path));
            }
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        toml::from_str(&text)
            .map_err(|e| ConfigError::Invalid(vec![Diagnostic::new(Some(path), e.to_string())]))
    }

    /// Where the workspace lives on disk. A git workspace must already be
    /// cloned into `~/.grove/workspace`.
    pub fn workspace_dir(home: &GroveHome, user: &UserConfig) -> Result<PathBuf, ConfigError> {
        let dir = match user.workspace_source() {
            None => return Err(ConfigError::NoWorkspace(home.config_file())),
            Some(WorkspaceSource::Path(p)) => p,
            Some(WorkspaceSource::Git(_)) => home.workspace_clone_dir(),
        };
        if !dir.join("grove.toml").exists() {
            return Err(ConfigError::WorkspaceMissing(dir));
        }
        Ok(dir)
    }

    pub fn idle_timeout(&self) -> Duration {
        self.user
            .idle_timeout
            .or(self.workspace.config.idle_timeout)
            .unwrap_or(DEFAULT_IDLE_TIMEOUT)
    }

    pub fn port_range(&self) -> (u16, u16) {
        self.user
            .port_range
            .or(self.workspace.config.port_range)
            .map(|[lo, hi]| (lo, hi))
            .unwrap_or(DEFAULT_PORT_RANGE)
    }

    pub fn postgres_url(&self) -> String {
        self.user
            .postgres
            .url
            .clone()
            .or_else(|| self.workspace.config.postgres.url.clone())
            .unwrap_or_else(|| DEFAULT_POSTGRES_URL.to_string())
    }

    pub fn domain_templates(&self) -> &DomainTemplates {
        &self.workspace.config.domains
    }

    pub fn editor(&self) -> Option<&str> {
        self.user.editor.as_deref()
    }

    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.get(name)
    }
}

/// Loads and validates a workspace directory. `overrides` are the user's
/// per-project overrides, deep-merged before validation.
pub fn load_workspace(
    dir: &Path,
    overrides: &IndexMap<String, toml::Table>,
) -> Result<LoadedWorkspace, ConfigError> {
    let mut diags = Vec::new();
    let mut warnings = Vec::new();

    let ws_file = dir.join("grove.toml");
    let config = match toml::from_str::<WorkspaceConfig>(&read(&ws_file)?) {
        Ok(c) => Some(c),
        Err(e) => {
            diags.push(Diagnostic::new(Some(ws_file.clone()), e.to_string()));
            None
        }
    };

    let projects_dir = dir.join("projects");
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&projects_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(ConfigError::Io {
                path: projects_dir,
                source,
            });
        }
    };
    files.sort();

    let mut projects: IndexMap<String, (ProjectConfig, PathBuf)> = IndexMap::new();
    let mut used_overrides = Vec::new();
    for file in files {
        let text = read(&file)?;
        let parsed = parse_project(&text, overrides, &mut used_overrides);
        match parsed {
            Ok(p) => {
                if let Some((_, other)) = projects.get(&p.name) {
                    diags.push(Diagnostic::new(
                        Some(file.clone()),
                        format!(
                            "project name `{}` is already used by {}",
                            p.name,
                            other.display()
                        ),
                    ));
                    continue;
                }
                projects.insert(p.name.clone(), (p, file));
            }
            Err(msg) => diags.push(Diagnostic::new(Some(file), msg)),
        }
    }

    for name in overrides.keys() {
        if !used_overrides.contains(name) {
            warnings.push(Diagnostic::new(
                None,
                format!("overrides for `{name}` don't match any workspace project"),
            ));
        }
    }

    let Some(config) = config else {
        return Err(ConfigError::Invalid(diags));
    };

    diags.extend(validate(&config, &projects, &ws_file));
    if !diags.is_empty() {
        return Err(ConfigError::Invalid(diags));
    }

    Ok(LoadedWorkspace {
        dir: dir.to_path_buf(),
        config,
        projects,
        warnings,
    })
}

fn parse_project(
    text: &str,
    overrides: &IndexMap<String, toml::Table>,
    used: &mut Vec<String>,
) -> Result<ProjectConfig, String> {
    let mut table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    match name.as_ref().and_then(|n| overrides.get(n)) {
        Some(ov) => {
            used.push(name.clone().unwrap());
            deep_merge(&mut table, ov.clone());
            toml::Value::Table(table)
                .try_into::<ProjectConfig>()
                .map_err(|e| format!("{e} (after applying your overrides)"))
        }
        // Parse from text again so errors carry line numbers.
        None => toml::from_str(text).map_err(|e| e.to_string()),
    }
}

/// Tables merge recursively; every other value (including arrays) replaces.
fn deep_merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => deep_merge(b, o),
            (_, v) => {
                base.insert(key, v);
            }
        }
    }
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_ws(dir: &Path) {
        fs::create_dir_all(dir.join("projects")).unwrap();
        fs::write(
            dir.join("grove.toml"),
            "name = \"acme\"\nidle_timeout = \"10m\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("projects/api.toml"),
            r#"
name = "api"
repo = "git@github.com:acme/api.git"
copy_from_original = [".env"]
[env]
FRONTEND_URL = "{{ projects.frontend.web.url }}"
[processes.web]
run = "pnpm dev"
port = { env = "PORT", default = 3000 }
domain = true
"#,
        )
        .unwrap();
        fs::write(
            dir.join("projects/frontend.toml"),
            r#"
name = "frontend"
repo = "git@github.com:acme/frontend.git"
[processes.web]
run = "pnpm dev"
port = "PORT"
domain = { wildcard = true }
depends_on = ["api.web"]
"#,
        )
        .unwrap();
    }

    #[test]
    fn loads_with_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        write_ws(tmp.path());
        let home = GroveHome::new(tmp.path().join("home"));
        let user: UserConfig = toml::from_str(
            r#"
            clones_root = "/src"
            projects = ["api"]
            [overrides.api]
            path = "/elsewhere/api"
            copy_from_original = [".env", "node_modules"]
            [overrides.api.processes.web]
            run = "pnpm dev --turbo"
            "#,
        )
        .unwrap();
        let cfg = Config::load_with(home, user, tmp.path()).unwrap();
        assert_eq!(cfg.workspace.projects.len(), 2);
        assert_eq!(cfg.projects.len(), 1);
        let api = cfg.project("api").unwrap();
        assert_eq!(api.main_clone, PathBuf::from("/elsewhere/api"));
        assert_eq!(api.config.copy_from_original.len(), 2);
        assert_eq!(api.config.processes["web"].run, "pnpm dev --turbo");
        // Merge keeps untouched keys.
        assert_eq!(
            api.config.processes["web"].port.as_ref().unwrap().default,
            Some(3000)
        );
        assert_eq!(cfg.idle_timeout(), Duration::from_secs(600));
    }

    #[test]
    fn default_clone_path() {
        let tmp = tempfile::tempdir().unwrap();
        write_ws(tmp.path());
        let user: UserConfig = toml::from_str("clones_root = \"/src\"").unwrap();
        let cfg = Config::load_with(GroveHome::new("/g"), user, tmp.path()).unwrap();
        assert_eq!(
            cfg.project("frontend").unwrap().main_clone,
            PathBuf::from("/src/frontend")
        );
    }

    #[test]
    fn reports_all_problems() {
        let tmp = tempfile::tempdir().unwrap();
        write_ws(tmp.path());
        fs::write(tmp.path().join("projects/bad.toml"), "name = \"bad\"\n").unwrap();
        fs::write(
            tmp.path().join("projects/dup.toml"),
            "name = \"api\"\nrepo = \"x\"\n",
        )
        .unwrap();
        let err = load_workspace(tmp.path(), &IndexMap::new()).unwrap_err();
        let diags = err.diagnostics();
        assert_eq!(diags.len(), 2, "{err}");
        assert!(err.to_string().contains("repo"), "{err}");
        assert!(err.to_string().contains("already used"), "{err}");
    }

    #[test]
    fn deep_merge_replaces_arrays() {
        let mut base: toml::Table = toml::from_str("a = [1, 2]\n[t]\nx = 1\ny = 2").unwrap();
        let over: toml::Table = toml::from_str("a = [3]\n[t]\ny = 3").unwrap();
        deep_merge(&mut base, over);
        assert_eq!(base["a"].as_array().unwrap().len(), 1);
        assert_eq!(base["t"]["x"].as_integer(), Some(1));
        assert_eq!(base["t"]["y"].as_integer(), Some(3));
    }
}
