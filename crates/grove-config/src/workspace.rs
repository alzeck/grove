use crate::duration;
use crate::project::DomainConfig;
use crate::template::{TemplateError, Templates};
use serde::Deserialize;
use std::time::Duration;

/// `grove.toml` at the root of a workspace.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub idle_timeout: Option<Duration>,
    pub port_range: Option<[u16; 2]>,
    #[serde(default)]
    pub domains: DomainTemplates,
    #[serde(default)]
    pub postgres: PostgresConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DomainTemplates {
    /// Host for the default cluster.
    #[serde(default = "default_domain_template")]
    pub default: String,
    /// Host for every other cluster.
    #[serde(default = "cluster_domain_template")]
    pub cluster: String,
}

impl Default for DomainTemplates {
    fn default() -> Self {
        Self {
            default: default_domain_template(),
            cluster: cluster_domain_template(),
        }
    }
}

impl DomainTemplates {
    /// Renders the host for one process. Process-level templates win over
    /// the workspace ones. The result is lowercased.
    pub fn render_host(
        &self,
        templates: &Templates,
        domain: &DomainConfig,
        project: &str,
        process: &str,
        cluster: &str,
        is_default_cluster: bool,
    ) -> Result<String, TemplateError> {
        let tpl = if is_default_cluster {
            domain.default.as_deref().unwrap_or(&self.default)
        } else {
            domain.cluster.as_deref().unwrap_or(&self.cluster)
        };
        let ctx = serde_json::json!({
            "project": project,
            "process": process,
            "cluster": cluster,
        });
        let host = templates.render(tpl, ctx)?.trim().to_ascii_lowercase();
        if !is_valid_host(&host) {
            return Err(TemplateError {
                template: tpl.to_string(),
                message: format!("`{host}` is not a valid host name"),
            });
        }
        Ok(host)
    }
}

fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn default_domain_template() -> String {
    "{{ project }}.localhost".into()
}

fn cluster_domain_template() -> String {
    "{{ project }}-{{ cluster }}.localhost".into()
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PostgresConfig {
    pub url: Option<String>,
}
