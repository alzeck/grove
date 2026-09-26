use async_trait::async_trait;
use std::time::Duration;

/// Whether a cluster can serve a request right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Ready,
    Starting,
    Idle,
    Stopped,
    Unknown,
}

/// The proxy's view of the orchestrator, implemented by grove-core.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn availability(&self, cluster: &str) -> Availability;
    /// Called for every proxied HTTP request (NOT for websocket traffic).
    fn record_activity(&self, cluster: &str);
    /// Start an idle cluster. Must not block.
    fn wake(&self, cluster: &str);
    async fn wait_until_ready(&self, cluster: &str, timeout: Duration) -> bool;
    /// For the unknown-host page.
    fn index(&self) -> Vec<IndexEntry>;
}

/// One cluster on the unknown-host page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub cluster: String,
    pub state: String,
    /// `(label, url)` pairs, e.g. `("api.web", "https://api.localhost")`.
    pub urls: Vec<(String, String)>,
}
