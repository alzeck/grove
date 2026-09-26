//! Serializable views of Grove's state, shared by the UI and the IPC/CLI.

use grove_config::{DbMode, Diagnostic};
use grove_git::{FileChange, StatusSummary};
use grove_github::{PrState, RepoRef};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub config: ConfigStatus,
    pub proxy: ProxyStatus,
    pub clusters: Vec<ClusterView>,
}

impl Snapshot {
    pub fn cluster(&self, name: &str) -> Option<&ClusterView> {
        self.clusters.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigStatus {
    #[default]
    NotConfigured,
    WorkspaceMissing {
        message: String,
    },
    Invalid {
        diagnostics: Vec<DiagnosticView>,
    },
    Loaded {
        workspace: String,
        workspace_dir: PathBuf,
        warnings: Vec<DiagnosticView>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticView {
    pub file: Option<PathBuf>,
    pub message: String,
}

impl From<&Diagnostic> for DiagnosticView {
    fn from(d: &Diagnostic) -> Self {
        Self {
            file: d.file.clone(),
            message: d.message.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProxyStatus {
    #[default]
    Disabled,
    Running {
        https: Vec<String>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClusterOrigin {
    Default,
    Branch {
        branch: String,
    },
    PullRequest {
        project: String,
        number: u64,
        url: String,
    },
    Manual,
}

/// Overall state of a cluster, derived from its processes and any
/// operation in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClusterPhase {
    Stopped,
    Starting,
    Running,
    /// Some processes crashed or failed while others run.
    Degraded,
    /// Stopped by the idle timer; wakes on the next request.
    Idle,
    Stopping,
    Creating {
        step: String,
    },
    TearingDown,
    /// Creation failed; retry or tear down.
    Error {
        message: String,
    },
}

impl ClusterPhase {
    pub fn label(&self) -> &str {
        match self {
            ClusterPhase::Stopped => "stopped",
            ClusterPhase::Starting => "starting",
            ClusterPhase::Running => "running",
            ClusterPhase::Degraded => "degraded",
            ClusterPhase::Idle => "idle",
            ClusterPhase::Stopping => "stopping",
            ClusterPhase::Creating { .. } => "creating",
            ClusterPhase::TearingDown => "tearing down",
            ClusterPhase::Error { .. } => "error",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ClusterPhase::Starting | ClusterPhase::Running | ClusterPhase::Degraded
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterView {
    pub name: String,
    pub is_default: bool,
    pub origin: ClusterOrigin,
    pub phase: ClusterPhase,
    pub created_at: u64,
    pub instances: Vec<InstanceView>,
    pub activities: Vec<ActivityView>,
}

impl ClusterView {
    pub fn instance(&self, project: &str) -> Option<&InstanceView> {
        self.instances.iter().find(|i| i.project == project)
    }

    pub fn urls(&self) -> Vec<(String, String)> {
        self.instances
            .iter()
            .flat_map(|i| {
                i.processes.iter().filter_map(move |p| {
                    p.url
                        .clone()
                        .map(|u| (format!("{}.{}", i.project, p.name), u))
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckoutView {
    /// The project's main clone (default cluster).
    MainClone { path: PathBuf, exists: bool },
    Worktree {
        path: PathBuf,
        branch: Option<String>,
        adopted: bool,
    },
    /// Uses the default cluster's instance.
    Reuse,
}

impl CheckoutView {
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            CheckoutView::MainClone { path, .. } | CheckoutView::Worktree { path, .. } => {
                Some(path)
            }
            CheckoutView::Reuse => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceView {
    pub project: String,
    pub checkout: CheckoutView,
    pub database: Option<DatabaseView>,
    pub pr: Option<PrLink>,
    /// Last known git status, refreshed on request.
    pub git: Option<StatusSummary>,
    pub processes: Vec<ProcessView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseView {
    pub mode: DbMode,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    pub repo: RepoRef,
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: Option<PrState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcState {
    #[default]
    Stopped,
    /// Waiting for dependencies.
    Waiting,
    Starting,
    Ready,
    /// Exited on its own.
    Exited {
        status: String,
        success: bool,
    },
    /// Couldn't start or never became ready.
    Failed {
        message: String,
    },
}

impl ProcState {
    pub fn label(&self) -> &str {
        match self {
            ProcState::Stopped => "stopped",
            ProcState::Waiting => "waiting",
            ProcState::Starting => "starting",
            ProcState::Ready => "ready",
            ProcState::Exited { success: true, .. } => "exited",
            ProcState::Exited { .. } => "crashed",
            ProcState::Failed { .. } => "failed",
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, ProcState::Starting | ProcState::Ready)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessView {
    pub name: String,
    pub state: ProcState,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub pid: Option<u32>,
    pub autostart: bool,
}

/// A one-off command Grove ran for a cluster (git, setup, migrations…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityView {
    pub id: u64,
    pub project: Option<String>,
    pub title: String,
    pub state: ActivityState,
    pub started_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityState {
    Running,
    Succeeded,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeardownReport {
    pub cluster: String,
    pub worktrees: Vec<WorktreeReport>,
    /// Fresh databases that will be dropped.
    pub databases: Vec<String>,
}

impl TeardownReport {
    pub fn has_dirty_worktrees(&self) -> bool {
        self.worktrees
            .iter()
            .any(|w| !w.changes.is_empty() && w.will_remove)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeReport {
    pub project: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub adopted: bool,
    pub will_remove: bool,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeardownOptions {
    /// Remove worktrees even if they have uncommitted changes.
    pub force: bool,
    /// Also remove worktrees that Grove adopted rather than created.
    pub remove_adopted: bool,
}

/// A worktree of a main clone that no cluster uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalWorktree {
    pub project: String,
    pub path: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoreEvent {
    /// Something in the snapshot changed; re-read it.
    Changed,
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}
