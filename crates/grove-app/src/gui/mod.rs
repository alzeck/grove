//! The desktop app: a GPUI window over [`grove_core::Core`], a menu bar item,
//! and the IPC server the CLI talks to.
//!
//! Threads: GPUI owns the main thread; the core, the proxy and the IPC server
//! run on a multi-threaded tokio runtime. Views call the core by spawning on
//! that runtime and awaiting the join handle from a GPUI task
//! ([`services::run_op`]); core events come back through [`store::Store`].

mod app;
mod cluster_page;
#[cfg(feature = "debug-screenshots")]
mod debug_shots;
mod logging;
mod new_cluster;
mod onboarding;
mod output;
mod platform;
mod quit;
mod services;
mod settings;
mod sidebar;
mod store;
mod teardown;
mod theme;
mod tray;
mod ui;
mod workspace;

pub use app::run;
