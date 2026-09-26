//! `grove headless`: the core, proxy and IPC server without a window.

use grove_config::GroveHome;
use grove_core::{Core, CoreEvent, CoreOptions, NoticeLevel};
use grove_proxy::ProxyConfig;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::ExitCode;

pub fn run(no_proxy: bool, https_port: u16, http_port: u16) -> ExitCode {
    init_tracing();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        let home = GroveHome::from_env();
        let mut opts = CoreOptions::new(home.clone());
        opts.proxy = (!no_proxy).then(|| proxy_config(https_port, http_port));

        let core = match Core::start(opts).await {
            Ok(core) => core,
            Err(e) => {
                eprintln!("grove: {e}");
                return ExitCode::FAILURE;
            }
        };
        let _server = match grove_ipc::serve(core.clone(), &home.socket_path(), None).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("grove: {e}");
                core.shutdown().await;
                return ExitCode::FAILURE;
            }
        };

        print_summary(&core);
        let mut events = core.subscribe();
        let notices = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let CoreEvent::Notice { level, message } = event {
                    let tag = match level {
                        NoticeLevel::Info => "info",
                        NoticeLevel::Warning => "warning",
                        NoticeLevel::Error => "error",
                    };
                    eprintln!("[{tag}] {message}");
                }
            }
        });

        wait_for_signal().await;
        eprintln!("stopping…");
        notices.abort();
        core.shutdown().await;
        ExitCode::SUCCESS
    })
}

pub fn proxy_config(https_port: u16, http_port: u16) -> ProxyConfig {
    let addrs = |port| {
        vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        ]
    };
    ProxyConfig {
        https_addrs: addrs(https_port),
        http_addrs: addrs(http_port),
        ..ProxyConfig::default()
    }
}

fn print_summary(core: &Core) {
    let snap = core.snapshot();
    eprintln!(
        "grove headless running (home {})",
        core.home().root().display()
    );
    match &snap.config {
        grove_core::ConfigStatus::Loaded { workspace, .. } => {
            eprintln!("workspace: {workspace}");
        }
        other => eprintln!("config: {other:?}"),
    }
    eprintln!("proxy: {:?}", snap.proxy);
    for c in &snap.clusters {
        eprintln!("cluster {} ({})", c.name, c.phase.label());
        for (label, url) in c.urls() {
            eprintln!("  {label}: {url}");
        }
    }
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("GROVE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init();
}
