//! Onboarding: writing the user config, fetching the workspace, cloning
//! projects.

use crate::{Core, CoreError, Result};
use grove_config::{Appearance, Config, UserConfig, WorkspaceSource, expand_tilde, load_workspace};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The user-config fields onboarding and settings edit. Everything else in
/// the file (overrides, port range…) is preserved.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfigDraft {
    pub workspace: String,
    pub clones_root: String,
    /// `None` enables every workspace project.
    pub projects: Option<Vec<String>>,
    pub editor: Option<String>,
    pub postgres_url: Option<String>,
    pub idle_timeout: Option<String>,
    #[serde(default)]
    pub appearance: Appearance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProject {
    pub name: String,
    pub repo: String,
    pub main_clone: PathBuf,
    pub cloned: bool,
    pub enabled: bool,
}

impl Core {
    pub fn user_config_draft(&self) -> UserConfigDraft {
        let user = Config::load_user(&self.inner.home).unwrap_or_default();
        UserConfigDraft {
            workspace: user.workspace.clone().unwrap_or_default(),
            clones_root: user
                .clones_root
                .clone()
                .unwrap_or_else(|| "~/Developer".into()),
            projects: user.projects.clone(),
            editor: user.editor.clone(),
            postgres_url: user.postgres.url.clone(),
            idle_timeout: user.idle_timeout.map(|d| humantime_like(d.as_secs())),
            appearance: user.appearance,
        }
    }

    /// Writes `~/.grove/config.toml`, keeping keys the draft doesn't cover,
    /// then reloads.
    pub fn write_user_config(&self, draft: &UserConfigDraft) -> Result<()> {
        let path = self.inner.home.config_file();
        let mut table: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();

        let mut set = |key: &str, value: Option<toml::Value>| match value {
            Some(v) => {
                table.insert(key.into(), v);
            }
            None => {
                table.remove(key);
            }
        };
        let nonempty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        set(
            "workspace",
            nonempty(&draft.workspace).map(toml::Value::String),
        );
        set(
            "clones_root",
            nonempty(&draft.clones_root).map(toml::Value::String),
        );
        set(
            "projects",
            draft
                .projects
                .as_ref()
                .map(|p| toml::Value::Array(p.iter().cloned().map(toml::Value::String).collect())),
        );
        set(
            "editor",
            draft
                .editor
                .as_deref()
                .and_then(nonempty)
                .map(toml::Value::String),
        );
        set(
            "idle_timeout",
            draft
                .idle_timeout
                .as_deref()
                .and_then(nonempty)
                .map(toml::Value::String),
        );
        set(
            "appearance",
            (draft.appearance != Appearance::System)
                .then(|| toml::Value::String(draft.appearance.as_str().into())),
        );
        let mut pg = table
            .get("postgres")
            .and_then(|v| v.as_table().cloned())
            .unwrap_or_default();
        match draft.postgres_url.as_deref().and_then(nonempty) {
            Some(url) => {
                pg.insert("url".into(), toml::Value::String(url));
            }
            None => {
                pg.remove("url");
            }
        }
        if pg.is_empty() {
            table.remove("postgres");
        } else {
            table.insert("postgres".into(), toml::Value::Table(pg));
        }

        let text = toml::to_string_pretty(&table)
            .map_err(|e| CoreError::Invalid(format!("can't write config: {e}")))?;
        // Validate before writing so a bad draft can't brick startup.
        toml::from_str::<UserConfig>(&text)
            .map_err(|e| CoreError::Invalid(format!("invalid settings: {e}")))?;
        std::fs::create_dir_all(self.inner.home.root())?;
        std::fs::write(&path, text)?;
        self.reload_config();
        Ok(())
    }

    /// Clones a git workspace into `~/.grove/workspace`, or fast-forwards
    /// it. Local workspaces are left alone.
    pub async fn sync_workspace(&self) -> Result<String> {
        let user = Config::load_user(&self.inner.home)
            .map_err(|e| CoreError::NotConfigured(e.to_string()))?;
        let message = match user.workspace_source() {
            None => return Err(CoreError::NotConfigured("no workspace set".into())),
            Some(WorkspaceSource::Path(p)) => {
                if !p.join("grove.toml").exists() {
                    return Err(CoreError::Invalid(format!(
                        "{} has no grove.toml",
                        p.display()
                    )));
                }
                format!("using local workspace {}", p.display())
            }
            Some(WorkspaceSource::Git(url)) => {
                let dir = self.inner.home.workspace_clone_dir();
                if dir.join(".git").exists() {
                    match grove_git::pull_ff_only(&dir).await {
                        Ok(_) => "workspace updated".to_string(),
                        Err(e) => format!("workspace not updated: {e}"),
                    }
                } else {
                    grove_git::clone(&url, &dir).await?;
                    "workspace cloned".to_string()
                }
            }
        };
        self.reload_config();
        Ok(message)
    }

    /// Every project in the workspace, whether enabled on this machine or
    /// not, for onboarding and settings.
    pub fn workspace_projects(&self) -> Result<Vec<WorkspaceProject>> {
        let user = Config::load_user(&self.inner.home)
            .map_err(|e| CoreError::NotConfigured(e.to_string()))?;
        let dir = Config::workspace_dir(&self.inner.home, &user)
            .map_err(|e| CoreError::NotConfigured(e.to_string()))?;
        let ws = load_workspace(&dir, &user.overrides)
            .map_err(|e| CoreError::NotConfigured(e.to_string()))?;
        let root = user.clones_root();
        Ok(ws
            .projects
            .values()
            .map(|(p, _)| {
                let main_clone = p
                    .path
                    .as_deref()
                    .map(expand_tilde)
                    .unwrap_or_else(|| root.join(&p.name));
                WorkspaceProject {
                    name: p.name.clone(),
                    repo: p.repo.clone(),
                    cloned: main_clone.join(".git").exists(),
                    main_clone,
                    enabled: user.is_enabled(&p.name),
                }
            })
            .collect())
    }

    /// Clones a project's repo to its main clone path.
    pub async fn clone_project(&self, name: &str) -> Result<()> {
        let project = self
            .workspace_projects()?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| CoreError::UnknownProject(name.into()))?;
        if project.cloned {
            return Ok(());
        }
        if let Some(parent) = project.main_clone.parent() {
            std::fs::create_dir_all(parent)?;
        }
        grove_git::clone(&project.repo, &project.main_clone).await?;
        self.reload_config();
        Ok(())
    }
}

fn humantime_like(secs: u64) -> String {
    match secs {
        0 => "0".into(),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}
