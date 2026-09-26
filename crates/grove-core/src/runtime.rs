//! In-memory runtime state: processes, activities, cached git status.

use crate::model::{ActivityState, ActivityView, ClusterPhase, ProcState};
use grove_git::StatusSummary;
use grove_proc::ManagedProcess;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

const MAX_ACTIVITIES: usize = 50;

#[derive(Default)]
pub(crate) struct Runtime {
    pub clusters: HashMap<String, ClusterRt>,
    /// Interactive shells handed to the UI; killed on shutdown.
    pub shells: Vec<ManagedProcess>,
}

impl Runtime {
    pub fn cluster(&mut self, name: &str) -> &mut ClusterRt {
        self.clusters.entry(name.to_string()).or_default()
    }
}

pub(crate) struct ClusterRt {
    /// An operation that overrides the derived phase.
    pub op: Option<ClusterPhase>,
    pub idle: bool,
    /// Bumped by stop; pending starts from an older epoch give up.
    pub epoch: u64,
    pub last_activity: Instant,
    pub procs: HashMap<(String, String), ProcRt>,
    pub activities: VecDeque<Activity>,
    pub git: HashMap<String, StatusSummary>,
}

impl Default for ClusterRt {
    fn default() -> Self {
        Self {
            op: None,
            idle: false,
            epoch: 0,
            last_activity: Instant::now(),
            procs: HashMap::new(),
            activities: VecDeque::new(),
            git: HashMap::new(),
        }
    }
}

impl ClusterRt {
    pub fn proc_mut(&mut self, project: &str, process: &str) -> &mut ProcRt {
        self.procs
            .entry((project.to_string(), process.to_string()))
            .or_default()
    }

    pub fn proc_state(&self, project: &str, process: &str) -> ProcState {
        self.procs
            .get(&(project.to_string(), process.to_string()))
            .map(|p| p.state.clone())
            .unwrap_or(ProcState::Stopped)
    }

    pub fn push_activity(&mut self, activity: Activity) {
        self.activities.push_back(activity);
        while self.activities.len() > MAX_ACTIVITIES {
            self.activities.pop_front();
        }
    }

    pub fn activity_mut(&mut self, id: u64) -> Option<&mut Activity> {
        self.activities.iter_mut().find(|a| a.id == id)
    }

    /// Phase from the processes that belong to this cluster.
    pub fn derive_phase(&self) -> ClusterPhase {
        if let Some(op) = &self.op {
            return op.clone();
        }
        if self.idle {
            return ClusterPhase::Idle;
        }
        let mut starting = false;
        let mut running = false;
        let mut failed = false;
        for p in self.procs.values() {
            match &p.state {
                ProcState::Waiting | ProcState::Starting => starting = true,
                ProcState::Ready => running = true,
                ProcState::Failed { .. } => failed = true,
                ProcState::Exited { success: false, .. } => failed = true,
                ProcState::Exited { success: true, .. } | ProcState::Stopped => {}
            }
            if p.stopping {
                return ClusterPhase::Stopping;
            }
        }
        match (starting, running, failed) {
            (true, _, _) => ClusterPhase::Starting,
            (false, _, true) => ClusterPhase::Degraded,
            (false, true, false) => ClusterPhase::Running,
            (false, false, false) => ClusterPhase::Stopped,
        }
    }
}

#[derive(Default)]
pub(crate) struct ProcRt {
    pub state: ProcState,
    pub handle: Option<ManagedProcess>,
    pub port: Option<u16>,
    /// Set while we're stopping it on purpose, so the exit isn't a crash.
    pub stopping: bool,
    /// Incremented on every start; stale exit monitors check it.
    pub generation: u64,
}

pub(crate) struct Activity {
    pub id: u64,
    pub project: Option<String>,
    pub title: String,
    pub state: ActivityState,
    pub started_at: u64,
    pub process: Option<ManagedProcess>,
}

impl Activity {
    pub fn view(&self) -> ActivityView {
        ActivityView {
            id: self.id,
            project: self.project.clone(),
            title: self.title.clone(),
            state: self.state.clone(),
            started_at: self.started_at,
        }
    }
}
