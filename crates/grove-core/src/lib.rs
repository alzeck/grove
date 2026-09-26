//! Grove's orchestrator: clusters, processes, worktrees, databases, the
//! proxy and idling. Headless; the GUI and the IPC server are both clients
//! of [`Core`].

mod activity;
mod backend;
mod context;
mod create;
mod doctor;
mod error;
mod idle;
mod model;
mod ops;
mod runtime;
mod setup;
mod state;
mod supervise;
mod teardown;

pub use create::{ClusterPlan, NewClusterSource, PlannedCheckout, ProjectPlan};
pub use error::CoreError;
pub use model::*;
pub use setup::{UserConfigDraft, WorkspaceProject};
pub use state::WorktreeSource;

pub use grove_config::DbMode;
pub use grove_proc::ManagedProcess;

use grove_config::{Config, ConfigError, GroveHome, Templates};
use grove_proxy::{CertAuthority, Proxy, ProxyConfig};
use parking_lot::Mutex;
use runtime::Runtime;
use state::State;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{broadcast, watch};

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct CoreOptions {
    pub home: GroveHome,
    /// `None` runs without the proxy (tests, `grove headless --no-proxy`).
    pub proxy: Option<ProxyConfig>,
    /// Kill process groups left over from a previous crash.
    pub reap_orphans: bool,
    /// Start the idle timer.
    pub idle: bool,
    /// How often the idle timer checks clusters.
    pub idle_tick: std::time::Duration,
}

impl CoreOptions {
    pub fn new(home: GroveHome) -> Self {
        Self {
            home,
            proxy: Some(ProxyConfig::default()),
            reap_orphans: true,
            idle: true,
            idle_tick: std::time::Duration::from_secs(15),
        }
    }
}

/// Handle to the running core. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    home: GroveHome,
    templates: Templates,
    config: Mutex<ConfigSlot>,
    state: Mutex<State>,
    /// Serialises writes of state.json.
    save_lock: Mutex<()>,
    rt: Mutex<Runtime>,
    events: broadcast::Sender<CoreEvent>,
    /// Bumped on every change; waiters re-check their condition.
    version: watch::Sender<u64>,
    env_cache: Mutex<HashMap<PathBuf, Arc<HashMap<String, String>>>>,
    ca: Option<Arc<CertAuthority>>,
    proxy: Mutex<Option<Proxy>>,
    proxy_status: Mutex<ProxyStatus>,
    /// Port the proxy serves HTTPS on, for building URLs.
    https_port: Mutex<u16>,
    op_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    activity_seq: AtomicU64,
    shutting_down: AtomicBool,
    shutdown_done: watch::Sender<bool>,
}

#[derive(Clone)]
pub(crate) enum ConfigSlot {
    Loaded(Arc<Config>),
    Missing(ConfigStatus),
}

impl Core {
    /// Loads config and state, reaps orphans, starts the proxy and the idle
    /// timer. Missing or invalid config is not an error: the snapshot
    /// reports it and the UI offers onboarding.
    pub async fn start(opts: CoreOptions) -> Result<Core> {
        std::fs::create_dir_all(opts.home.root())?;
        let state = State::load(&opts.home.state_file());

        if opts.reap_orphans && !state.running.is_empty() {
            let reaped =
                grove_proc::reap_orphans(&state.running, std::time::Duration::from_secs(5)).await;
            if reaped > 0 {
                tracing::warn!("stopped {reaped} process group(s) left over from a previous run");
            }
        }

        let ca = match CertAuthority::load_or_create(&opts.home.ca_dir()) {
            Ok(ca) => Some(Arc::new(ca)),
            Err(e) => {
                tracing::error!("certificate authority unavailable: {e}");
                None
            }
        };

        let (events, _) = broadcast::channel(256);
        let (version, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            home: opts.home.clone(),
            templates: Templates::new(),
            config: Mutex::new(load_config(&opts.home)),
            state: Mutex::new(State {
                running: Vec::new(),
                ..state
            }),
            save_lock: Mutex::new(()),
            rt: Mutex::new(Runtime::default()),
            events,
            version,
            env_cache: Mutex::new(HashMap::new()),
            ca,
            proxy: Mutex::new(None),
            proxy_status: Mutex::new(ProxyStatus::Disabled),
            https_port: Mutex::new(443),
            op_locks: Mutex::new(HashMap::new()),
            activity_seq: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            shutdown_done: watch::channel(false).0,
        });
        let core = Core { inner };
        core.save_state();
        core.ensure_default_ports();

        if let Some(proxy_config) = opts.proxy {
            core.start_proxy(proxy_config).await;
        }
        if opts.idle {
            idle::spawn(core.clone(), opts.idle_tick);
        }
        Ok(core)
    }

    pub fn home(&self) -> &GroveHome {
        &self.inner.home
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.inner.events.subscribe()
    }

    /// Current configuration, if loaded.
    pub fn config(&self) -> Option<Arc<Config>> {
        match &*self.inner.config.lock() {
            ConfigSlot::Loaded(c) => Some(c.clone()),
            ConfigSlot::Missing(_) => None,
        }
    }

    pub(crate) fn require_config(&self) -> Result<Arc<Config>> {
        match &*self.inner.config.lock() {
            ConfigSlot::Loaded(c) => Ok(c.clone()),
            ConfigSlot::Missing(status) => Err(CoreError::NotConfigured(match status {
                ConfigStatus::Invalid { diagnostics } => diagnostics
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
                ConfigStatus::WorkspaceMissing { message } => message.clone(),
                _ => "run onboarding or create ~/.grove/config.toml".into(),
            })),
        }
    }

    /// Re-reads config from disk. Running processes keep their old settings
    /// until restarted.
    pub fn reload_config(&self) -> ConfigStatus {
        let slot = load_config(&self.inner.home);
        *self.inner.config.lock() = slot;
        self.ensure_default_ports();
        self.update_routes();
        self.notify();
        self.snapshot().config
    }

    pub(crate) fn notify(&self) {
        self.inner.version.send_modify(|v| *v += 1);
        let _ = self.inner.events.send(CoreEvent::Changed);
    }

    pub(crate) fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            NoticeLevel::Error => tracing::error!("{message}"),
            NoticeLevel::Warning => tracing::warn!("{message}"),
            NoticeLevel::Info => tracing::info!("{message}"),
        }
        let _ = self.inner.events.send(CoreEvent::Notice { level, message });
    }

    pub(crate) fn save_state(&self) {
        let _saving = self.inner.save_lock.lock();
        let state = self.inner.state.lock().clone();
        if let Err(e) = state.save(&self.inner.home.state_file()) {
            tracing::error!("can't save state: {e}");
        }
    }

    pub(crate) fn op_lock(&self, cluster: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .op_locks
            .lock()
            .entry(cluster.to_string())
            .or_default()
            .clone()
    }

    /// Waits until `f` returns `Some`, re-checking after every change.
    pub(crate) async fn wait_for<T>(&self, mut f: impl FnMut(&Core) -> Option<T>) -> T {
        let mut rx = self.inner.version.subscribe();
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            if rx.changed().await.is_err() {
                // Sender lives in Inner, which we hold; can't happen.
                std::future::pending::<()>().await;
            }
        }
    }

    pub fn ca_cert_path(&self) -> Option<PathBuf> {
        self.inner.ca.as_ref().map(|ca| ca.cert_path())
    }

    pub fn ca(&self) -> Option<Arc<CertAuthority>> {
        self.inner.ca.clone()
    }

    async fn start_proxy(&self, config: ProxyConfig) {
        let Some(ca) = self.inner.ca.clone() else {
            *self.inner.proxy_status.lock() = ProxyStatus::Failed {
                message: "no certificate authority".into(),
            };
            return;
        };
        let backend = Arc::new(backend::CoreBackend::new(self));
        match Proxy::start(config, ca, backend).await {
            Ok(proxy) => {
                let (https, _) = proxy.local_addrs();
                if let Some(addr) = https.first() {
                    *self.inner.https_port.lock() = addr.port();
                }
                *self.inner.proxy_status.lock() = ProxyStatus::Running {
                    https: https.iter().map(|a| a.to_string()).collect(),
                };
                *self.inner.proxy.lock() = Some(proxy);
                self.update_routes();
            }
            Err(e) => {
                *self.inner.proxy_status.lock() = ProxyStatus::Failed {
                    message: e.to_string(),
                };
                self.notice(NoticeLevel::Error, format!("proxy didn't start: {e}"));
            }
        }
        self.notify();
    }

    /// Retries starting the proxy (e.g. after freeing port 443).
    pub async fn restart_proxy(&self, config: ProxyConfig) {
        let old = self.inner.proxy.lock().take();
        if let Some(old) = old {
            old.shutdown().await;
        }
        self.start_proxy(config).await;
    }

    /// Stops every process and the proxy. Call before exiting. Concurrent
    /// calls all wait until the first one has finished.
    pub async fn shutdown(&self) {
        let mut done = self.inner.shutdown_done.subscribe();
        if self.inner.shutting_down.swap(true, Ordering::SeqCst) {
            let _ = done.wait_for(|finished| *finished).await;
            return;
        }
        self.stop_everything().await;
        let shells = std::mem::take(&mut self.inner.rt.lock().shells);
        futures::future::join_all(
            shells
                .iter()
                .map(|s| s.terminate(std::time::Duration::from_secs(2))),
        )
        .await;
        let proxy = self.inner.proxy.lock().take();
        if let Some(proxy) = proxy {
            proxy.shutdown().await;
        }
        self.save_state();
        self.inner.shutdown_done.send_replace(true);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }
}

fn load_config(home: &GroveHome) -> ConfigSlot {
    match Config::load(home) {
        Ok(c) => ConfigSlot::Loaded(Arc::new(c)),
        Err(ConfigError::NotConfigured(_) | ConfigError::NoWorkspace(_)) => {
            ConfigSlot::Missing(ConfigStatus::NotConfigured)
        }
        Err(e @ ConfigError::WorkspaceMissing(_)) => {
            ConfigSlot::Missing(ConfigStatus::WorkspaceMissing {
                message: e.to_string(),
            })
        }
        Err(e) => ConfigSlot::Missing(ConfigStatus::Invalid {
            diagnostics: e.diagnostics().iter().map(Into::into).collect(),
        }),
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
