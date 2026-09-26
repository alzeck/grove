//! Stops clusters nobody has visited for `idle_timeout`. They wake on the
//! next request through the proxy.

use crate::{ClusterPhase, Core};
use grove_config::DEFAULT_CLUSTER;
use std::time::Duration;

pub(crate) fn spawn(core: Core, tick: Duration) {
    let weak = std::sync::Arc::downgrade(&core.inner);
    drop(core);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(inner) = weak.upgrade() else { return };
            let core = Core { inner };
            if core.is_shutting_down() {
                return;
            }
            for cluster in idle_candidates(&core) {
                tracing::info!("{cluster} is idle; stopping it");
                core.idle_cluster(&cluster).await;
            }
        }
    });
}

fn idle_candidates(core: &Core) -> Vec<String> {
    let Some(cfg) = core.config() else {
        return Vec::new();
    };
    let timeout = cfg.idle_timeout();
    if timeout.is_zero() {
        return Vec::new();
    }
    let names = core.cluster_names();
    let expired: Vec<String> = {
        let mut rt = core.inner.rt.lock();
        names
            .into_iter()
            .filter(|n| n != DEFAULT_CLUSTER)
            .filter(|n| {
                let crt = rt.cluster(n);
                crt.op.is_none()
                    && matches!(
                        crt.derive_phase(),
                        ClusterPhase::Running | ClusterPhase::Degraded
                    )
                    && crt.last_activity.elapsed() >= timeout
            })
            .collect()
    };
    // Without a domain there's no way to wake it, so leave it running.
    expired
        .into_iter()
        .filter(|n| core.cluster_has_routes(n))
        .collect()
}
