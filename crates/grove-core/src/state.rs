//! What survives restarts: clusters Grove created, their port allocations,
//! and the process groups that were running (for crash recovery).

use crate::model::{ClusterOrigin, PrLink};
use grove_config::DbMode;
use grove_proc::PidRecord;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub clusters: Vec<ClusterRecord>,
    /// `cluster/project/process` → port.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    #[serde(default)]
    pub running: Vec<PidRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterRecord {
    pub name: String,
    pub created_at: u64,
    pub origin: ClusterOrigin,
    pub instances: IndexMap<String, InstanceRecord>,
    /// Set while creation hasn't finished; cleared once every step is done.
    #[serde(default)]
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub checkout: CheckoutRecord,
    #[serde(default)]
    pub db: Option<DbRecord>,
    #[serde(default)]
    pub pr: Option<PrLink>,
    /// Worktree, copy and setup steps are done.
    #[serde(default)]
    pub prepared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckoutRecord {
    Reuse,
    Worktree {
        path: PathBuf,
        branch: Option<String>,
        /// Created outside Grove; not removed on teardown by default.
        adopted: bool,
        /// How to create it if it doesn't exist yet.
        #[serde(default)]
        create: Option<WorktreeSource>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeSource {
    /// Existing local or origin branch.
    Branch { name: String },
    /// New branch from `base`.
    NewBranch { name: String, base: String },
    /// Fetch `remote_ref` from origin into `local_branch` first.
    Fetch {
        remote_ref: String,
        local_branch: String,
        /// Same-repo PRs track the origin branch so you can push.
        track: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbRecord {
    pub mode: DbMode,
    pub name: String,
    #[serde(default)]
    pub created: bool,
}

pub fn port_key(cluster: &str, project: &str, process: &str) -> String {
    format!("{cluster}/{project}/{process}")
}

impl State {
    pub fn load(path: &Path) -> State {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("ignoring unreadable {}: {e}", path.display());
                    let backup = path.with_extension("json.bad");
                    let _ = std::fs::rename(path, backup);
                    State::default()
                }
            },
            Err(_) => State::default(),
        }
    }

    /// Atomic write: temp file then rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)
    }

    pub fn cluster(&self, name: &str) -> Option<&ClusterRecord> {
        self.clusters.iter().find(|c| c.name == name)
    }

    pub fn cluster_mut(&mut self, name: &str) -> Option<&mut ClusterRecord> {
        self.clusters.iter_mut().find(|c| c.name == name)
    }

    pub fn remove_cluster(&mut self, name: &str) {
        self.clusters.retain(|c| c.name != name);
        let prefix = format!("{name}/");
        self.ports.retain(|k, _| !k.starts_with(&prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        s.clusters.push(ClusterRecord {
            name: "pr-1".into(),
            created_at: 1,
            origin: ClusterOrigin::Branch {
                branch: "feat".into(),
            },
            instances: IndexMap::from([(
                "api".to_string(),
                InstanceRecord {
                    checkout: CheckoutRecord::Worktree {
                        path: "/w".into(),
                        branch: Some("feat".into()),
                        adopted: false,
                        create: Some(WorktreeSource::Branch {
                            name: "feat".into(),
                        }),
                    },
                    db: Some(DbRecord {
                        mode: DbMode::Fresh,
                        name: "api_dev_pr_1".into(),
                        created: true,
                    }),
                    pr: None,
                    prepared: true,
                },
            )]),
            incomplete: false,
        });
        s.ports.insert(port_key("pr-1", "api", "web"), 4100);
        s.ports.insert(port_key("pr-10", "api", "web"), 4101);
        s.save(&path).unwrap();

        let mut loaded = State::load(&path);
        assert_eq!(loaded.clusters.len(), 1);
        loaded.remove_cluster("pr-1");
        assert!(loaded.cluster("pr-1").is_none());
        assert_eq!(loaded.ports.len(), 1, "must not remove pr-10's ports");
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{nope").unwrap();
        let s = State::load(&path);
        assert!(s.clusters.is_empty());
        assert!(dir.path().join("state.json.bad").exists());
    }
}
