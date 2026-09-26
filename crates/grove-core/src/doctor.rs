//! Environment checks shown in the diagnostics panel and `grove doctor`.

use crate::model::{DoctorCheck, ProxyStatus};
use crate::{ConfigStatus, Core};

fn check(name: &str, ok: bool, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

impl Core {
    pub async fn doctor(&self) -> Vec<DoctorCheck> {
        let mut out = Vec::new();

        out.push(match grove_git::git_version().await {
            Ok(v) => check("git", true, v),
            Err(e) => check("git", false, e.to_string()),
        });
        out.push(match grove_github::gh_version().await {
            Ok(v) => match grove_github::auth_status().await {
                Ok(()) => check("GitHub CLI", true, format!("{v}, logged in")),
                Err(e) => check(
                    "GitHub CLI",
                    false,
                    format!("{v}, {e}; run `gh auth login`"),
                ),
            },
            Err(e) => check(
                "GitHub CLI",
                false,
                format!("{e}; install with `brew install gh`"),
            ),
        });

        match self.snapshot().config {
            ConfigStatus::Loaded {
                workspace,
                warnings,
                ..
            } => {
                let detail = if warnings.is_empty() {
                    format!("workspace `{workspace}`")
                } else {
                    format!(
                        "workspace `{workspace}`; {}",
                        warnings
                            .iter()
                            .map(|w| w.message.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                };
                out.push(check("config", warnings.is_empty(), detail));
            }
            ConfigStatus::NotConfigured => {
                out.push(check("config", false, "not configured yet"));
            }
            ConfigStatus::WorkspaceMissing { message } => {
                out.push(check("config", false, message));
            }
            ConfigStatus::Invalid { diagnostics } => out.push(check(
                "config",
                false,
                diagnostics
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            )),
        }

        if let Some(cfg) = self.config() {
            let needs_pg = cfg.projects.values().any(|p| p.config.database.is_some());
            if needs_pg {
                let pg = grove_db::Postgres::new(cfg.postgres_url());
                out.push(match pg.ping().await {
                    Ok(v) => check(
                        "Postgres",
                        true,
                        format!("{} (server {v})", cfg.postgres_url()),
                    ),
                    Err(e) => check("Postgres", false, e.to_string()),
                });
                let tools = ["pg_dump", "pg_restore"]
                    .iter()
                    .map(|t| (t, grove_db::find_pg_tool(t)))
                    .collect::<Vec<_>>();
                let missing: Vec<_> = tools
                    .iter()
                    .filter(|(_, p)| p.is_none())
                    .map(|(t, _)| **t)
                    .collect();
                out.push(if missing.is_empty() {
                    check("Postgres tools", true, "pg_dump and pg_restore found")
                } else {
                    check(
                        "Postgres tools",
                        false,
                        format!("missing {}; `brew install libpq`", missing.join(", ")),
                    )
                });
            }
            for proj in cfg.projects.values() {
                let exists = proj.main_clone.join(".git").exists();
                out.push(check(
                    &format!("clone: {}", proj.name()),
                    exists,
                    if exists {
                        proj.main_clone.display().to_string()
                    } else {
                        format!("missing at {}", proj.main_clone.display())
                    },
                ));
            }
        }

        out.push(match self.snapshot().proxy {
            ProxyStatus::Running { https } => check("proxy", true, https.join(", ")),
            ProxyStatus::Disabled => check("proxy", false, "not running"),
            ProxyStatus::Failed { message } => check("proxy", false, message),
        });
        out.push(match &self.inner.ca {
            Some(ca) if ca.is_trusted() => check("HTTPS certificate", true, "trusted"),
            Some(_) => check(
                "HTTPS certificate",
                false,
                "Grove's certificate authority isn't trusted yet",
            ),
            None => check("HTTPS certificate", false, "no certificate authority"),
        });
        out
    }
}
