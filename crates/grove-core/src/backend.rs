//! What the proxy asks the core: is a cluster ready, wake it, record use.

use crate::model::ClusterPhase;
use crate::{Core, Inner};
use async_trait::async_trait;
use grove_proxy::{Availability, Backend, IndexEntry};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Holds a weak reference: the core owns the proxy, which owns this.
pub(crate) struct CoreBackend {
    inner: Weak<Inner>,
}

impl CoreBackend {
    pub fn new(core: &Core) -> Self {
        Self {
            inner: Arc::downgrade(&core.inner),
        }
    }

    fn core(&self) -> Option<Core> {
        self.inner.upgrade().map(|inner| Core { inner })
    }
}

#[async_trait]
impl Backend for CoreBackend {
    fn availability(&self, cluster: &str) -> Availability {
        let Some(core) = self.core() else {
            return Availability::Unknown;
        };
        if core.require_cluster(cluster).is_err() {
            return Availability::Unknown;
        }
        match core.phase(cluster) {
            ClusterPhase::Running | ClusterPhase::Degraded => Availability::Ready,
            ClusterPhase::Starting | ClusterPhase::Creating { .. } => Availability::Starting,
            ClusterPhase::Idle => Availability::Idle,
            ClusterPhase::Stopped
            | ClusterPhase::Stopping
            | ClusterPhase::TearingDown
            | ClusterPhase::Error { .. } => Availability::Stopped,
        }
    }

    fn record_activity(&self, cluster: &str) {
        if let Some(core) = self.core() {
            core.inner.rt.lock().cluster(cluster).last_activity = Instant::now();
        }
    }

    fn wake(&self, cluster: &str) {
        let Some(core) = self.core() else { return };
        if core.phase(cluster) != ClusterPhase::Idle {
            return;
        }
        let name = cluster.to_string();
        tokio::spawn(async move {
            if let Err(e) = core.start_cluster(&name).await {
                tracing::warn!("waking {name}: {e}");
            }
        });
    }

    async fn wait_until_ready(&self, cluster: &str, timeout: Duration) -> bool {
        let Some(core) = self.core() else {
            return false;
        };
        let wait = core.wait_for(|c| match c.phase(cluster) {
            ClusterPhase::Running | ClusterPhase::Degraded => Some(true),
            ClusterPhase::Stopped | ClusterPhase::Error { .. } | ClusterPhase::TearingDown => {
                Some(false)
            }
            _ => None,
        });
        tokio::time::timeout(timeout, wait).await.unwrap_or(false)
    }

    fn index(&self) -> Vec<IndexEntry> {
        let Some(core) = self.core() else {
            return Vec::new();
        };
        core.snapshot()
            .clusters
            .iter()
            .map(|c| IndexEntry {
                cluster: c.name.clone(),
                state: c.phase.label().to_string(),
                urls: c.urls(),
            })
            .collect()
    }
}
