use crate::duration;
use crate::{DEFAULT_READY_TIMEOUT, DEFAULT_STOP_TIMEOUT};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, de};
use std::fmt;
use std::time::Duration;

/// Process names that would clash with template variables
/// (`projects.<p>.dir`, `.db`, `.branch`).
pub const RESERVED_PROCESS_NAMES: &[&str] = &["dir", "db", "branch"];

/// `projects/<name>.toml`, after personal overrides are merged in.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    pub repo: String,
    pub default_branch: Option<String>,
    /// Main clone path. Normally only set through user overrides.
    pub path: Option<String>,
    /// Paths or globs copied from the main clone into new worktrees.
    #[serde(default)]
    pub copy_from_original: Vec<String>,
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub after_pull: Vec<String>,
    #[serde(default)]
    pub teardown: Vec<String>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub database: Option<DatabaseConfig>,
    #[serde(default)]
    pub processes: IndexMap<String, ProcessConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// The shared database.
    pub name: String,
    /// Env var that receives the connection URL.
    #[serde(default = "default_db_env")]
    pub env: String,
    #[serde(default)]
    pub default: DbMode,
    #[serde(default)]
    pub fresh: FreshStrategy,
    pub migrate: Option<String>,
    pub seed: Option<String>,
}

fn default_db_env() -> String {
    "DATABASE_URL".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DbMode {
    #[default]
    Shared,
    Fresh,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FreshStrategy {
    /// `pg_dump | pg_restore` from the shared database.
    #[default]
    Dump,
    /// `CREATE DATABASE … TEMPLATE shared`.
    Template,
    /// Empty database, then `migrate` and `seed`.
    Migrate,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    pub run: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    pub port: Option<PortConfig>,
    pub ready: Option<ReadyCheck>,
    #[serde(default, deserialize_with = "deserialize_domain")]
    pub domain: Option<DomainConfig>,
    #[serde(default = "default_true")]
    pub autostart: bool,
    #[serde(default)]
    pub depends_on: Vec<ProcessRef>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    stop_timeout: Option<Duration>,
}

impl ProcessConfig {
    pub fn stop_timeout(&self) -> Duration {
        self.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT)
    }
}

fn default_cwd() -> String {
    ".".into()
}

fn default_true() -> bool {
    true
}

/// `port = "PORT"` or `port = { env = "PORT", default = 3000 }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PortRaw")]
pub struct PortConfig {
    pub env: String,
    /// Fixed port for the default cluster.
    pub default: Option<u16>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PortRaw {
    Env(String),
    Table(PortTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortTable {
    #[serde(default = "default_port_env")]
    env: String,
    default: Option<u16>,
}

fn default_port_env() -> String {
    "PORT".into()
}

impl TryFrom<PortRaw> for PortConfig {
    type Error = String;
    fn try_from(raw: PortRaw) -> Result<Self, String> {
        let (env, default) = match raw {
            PortRaw::Env(env) => (env, None),
            PortRaw::Table(t) => (t.env, t.default),
        };
        if env.is_empty() {
            return Err("port env var name must not be empty".into());
        }
        Ok(PortConfig { env, default })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyKind {
    /// 2xx/3xx from `http://127.0.0.1:<port><path>`.
    Http { path: String },
    /// The port accepts TCP connections.
    Tcp,
    /// Output (ANSI stripped) matches this regex.
    Log { pattern: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "ReadyRaw")]
pub struct ReadyCheck {
    pub kind: ReadyKind,
    pub timeout: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyRaw {
    http: Option<String>,
    tcp: Option<bool>,
    log: Option<String>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    timeout: Option<Duration>,
}

impl TryFrom<ReadyRaw> for ReadyCheck {
    type Error = String;
    fn try_from(raw: ReadyRaw) -> Result<Self, String> {
        let mut kinds = Vec::new();
        if let Some(path) = raw.http {
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            kinds.push(ReadyKind::Http { path });
        }
        if raw.tcp == Some(true) {
            kinds.push(ReadyKind::Tcp);
        }
        if let Some(pattern) = raw.log {
            regex_check(&pattern)?;
            kinds.push(ReadyKind::Log { pattern });
        }
        if kinds.len() != 1 {
            return Err("`ready` needs exactly one of `http`, `tcp = true`, or `log`".into());
        }
        Ok(ReadyCheck {
            kind: kinds.pop().unwrap(),
            timeout: raw.timeout.unwrap_or(DEFAULT_READY_TIMEOUT),
        })
    }
}

fn regex_check(pattern: &str) -> Result<(), String> {
    regex::Regex::new(pattern)
        .map(|_| ())
        .map_err(|e| format!("invalid `ready.log` regex: {e}"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainConfig {
    /// Also route `*.<host>` to this process.
    #[serde(default)]
    pub wildcard: bool,
    /// Overrides the workspace template for the default cluster.
    pub default: Option<String>,
    /// Overrides the workspace template for other clusters.
    pub cluster: Option<String>,
}

fn deserialize_domain<'de, D>(d: D) -> Result<Option<DomainConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Flag(bool),
        Table(DomainConfig),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None | Some(Raw::Flag(false)) => None,
        Some(Raw::Flag(true)) => Some(DomainConfig::default()),
        Some(Raw::Table(t)) => Some(t),
    })
}

/// A dependency: `"web"` (same project) or `"api.web"` (another project in
/// the same cluster).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcessRef {
    pub project: Option<String>,
    pub process: String,
}

impl ProcessRef {
    pub fn resolve_project<'a>(&'a self, current: &'a str) -> &'a str {
        self.project.as_deref().unwrap_or(current)
    }
}

impl fmt::Display for ProcessRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.project {
            Some(p) => write!(f, "{p}.{}", self.process),
            None => f.write_str(&self.process),
        }
    }
}

impl<'de> Deserialize<'de> for ProcessRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.split_once('.') {
            Some((p, proc_)) if !p.is_empty() && !proc_.is_empty() && !proc_.contains('.') => {
                Ok(ProcessRef {
                    project: Some(p.to_string()),
                    process: proc_.to_string(),
                })
            }
            None if !s.is_empty() => Ok(ProcessRef {
                project: None,
                process: s,
            }),
            _ => Err(de::Error::custom(format!(
                "invalid dependency `{s}`: use `process` or `project.process`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = r#"
        name = "api"
        repo = "git@github.com:acme/api.git"
        copy_from_original = [".env", "node_modules"]
        setup = ["pnpm install"]

        [env]
        FRONTEND_URL = "{{ projects.frontend.web.url }}"

        [database]
        name = "api_dev"
        fresh = "template"
        migrate = "pnpm db:migrate"

        [processes.web]
        run = "pnpm dev"
        port = { env = "PORT", default = 3000 }
        ready = { http = "health", timeout = "30s" }
        domain = { wildcard = true }

        [processes.worker]
        run = "pnpm worker"
        port = "WORKER_PORT"
        depends_on = ["web", "frontend.web"]
        domain = false
        stop_timeout = "3s"
    "#;

    #[test]
    fn parses_project() {
        let p: ProjectConfig = toml::from_str(API).unwrap();
        let db = p.database.as_ref().unwrap();
        assert_eq!(db.env, "DATABASE_URL");
        assert_eq!(db.default, DbMode::Shared);
        assert_eq!(db.fresh, FreshStrategy::Template);

        let web = &p.processes["web"];
        assert_eq!(web.port.as_ref().unwrap().default, Some(3000));
        assert_eq!(
            web.ready.as_ref().unwrap().kind,
            ReadyKind::Http {
                path: "/health".into()
            }
        );
        assert!(web.domain.as_ref().unwrap().wildcard);
        assert!(web.autostart);
        assert_eq!(web.stop_timeout(), DEFAULT_STOP_TIMEOUT);

        let worker = &p.processes["worker"];
        assert_eq!(worker.port.as_ref().unwrap().env, "WORKER_PORT");
        assert!(worker.domain.is_none());
        assert_eq!(worker.depends_on[1].project.as_deref(), Some("frontend"));
        assert_eq!(worker.stop_timeout(), Duration::from_secs(3));
    }

    #[test]
    fn rejects_ambiguous_ready() {
        let err = toml::from_str::<ReadyCheck>(
            r#"http = "/"
tcp = true"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = toml::from_str::<ProjectConfig>("name='a'\nrepo='r'\nbogus=1").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }
}
