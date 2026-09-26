//! Startup: runtime, core, IPC server, GPUI application, window, menus.

use super::services::{MainWindow, Services, run_op};
use super::ui;
use super::workspace::Workspace;
use super::{logging, platform, quit, sidebar, store, theme, tray};
use gpui_kit::component::Root;
use gpui_kit::prelude::*;
use gpui_kit::{
    App, AsyncApp, Global, KeyBinding, Menu, MenuItem, OsAction, QuitMode, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size,
};
use grove_config::GroveHome;
use grove_core::{Core, CoreOptions};
use grove_ipc::{Request, ServerHandle};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

gpui_kit::actions!(
    grove,
    [
        /// Quit Grove (asks first if anything is running).
        Quit,
        /// Close the main window; Grove keeps running in the menu bar.
        CloseWindow,
        /// Show the main window.
        ShowWindow,
        /// Open the new-cluster dialog.
        NewCluster,
        /// Open settings.
        OpenSettings,
        /// Hide Grove.
        Hide,
    ]
);

/// Requests from outside GPUI (IPC, signals, the menu bar item).
pub enum UiCommand {
    ShowWindow,
    /// The core already shut down (e.g. SIGTERM); quit right away.
    Exit,
    /// A menu bar item was chosen.
    Tray(String),
}

/// Keeps the IPC server alive; dropped before quitting to remove the socket.
struct IpcServer(Option<ServerHandle>);

impl Global for IpcServer {}

pub fn run(background: bool) -> ExitCode {
    let home = GroveHome::from_env();
    logging::init(&home);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("grove-core")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("can't start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Single instance: hand over to the running app.
    let socket = home.socket_path();
    if runtime.block_on(grove_ipc::is_running(&socket)) {
        if !background {
            let _ = runtime.block_on(grove_ipc::call(&socket, &Request::ShowWindow, |_| {}));
        }
        tracing::info!("Grove is already running");
        return ExitCode::SUCCESS;
    }

    let (https_port, http_port) = ports_from_env();
    let mut opts = CoreOptions::new(home.clone());
    opts.proxy = Some(crate::headless::proxy_config(https_port, http_port));
    let core = match runtime.block_on(Core::start(opts)) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!("Grove can't start: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (commands_tx, commands_rx) = mpsc::unbounded_channel::<UiCommand>();
    let show_tx = commands_tx.clone();
    let show: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = show_tx.send(UiCommand::ShowWindow);
    });
    let server = match runtime.block_on(grove_ipc::serve(core.clone(), &socket, Some(show))) {
        Ok(server) => server,
        Err(e) => {
            tracing::error!("can't listen on {}: {e}", socket.display());
            runtime.block_on(core.shutdown());
            return ExitCode::FAILURE;
        }
    };
    runtime.spawn(stop_on_signal(core.clone(), commands_tx.clone()));
    tracing::info!(
        "Grove started (home {}, https port {https_port})",
        home.root().display()
    );

    let services = Services {
        core: core.clone(),
        rt: runtime.handle().clone(),
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            grove_term::init(cx);
            theme::init(services.core.user_config_draft().appearance, cx);
            cx.set_global(services);
            cx.set_global(IpcServer(Some(server)));
            cx.set_global(MainWindow::default());
            store::start(cx);
            init_actions(cx);
            tray::init(cx, commands_tx);
            pump_commands(cx, commands_rx);
            watch_window_close(cx);
            stop_core_on_quit(cx);
            if background {
                platform::set_dock_icon_visible(false);
            } else {
                show_main_window(cx);
            }
            #[cfg(feature = "debug-screenshots")]
            super::debug_shots::start(cx);
        });

    // Only reached if the platform's run loop returns instead of exiting.
    runtime.block_on(core.shutdown());
    ExitCode::SUCCESS
}

/// `GROVE_HTTPS_PORT` / `GROVE_HTTP_PORT`, for trying Grove without 443/80.
fn ports_from_env() -> (u16, u16) {
    let port = |name: &str, default: u16| match std::env::var(name) {
        Ok(v) => v.trim().parse().unwrap_or_else(|_| {
            tracing::warn!("{name}={v:?} isn't a port; using {default}");
            default
        }),
        Err(_) => default,
    };
    (port("GROVE_HTTPS_PORT", 443), port("GROVE_HTTP_PORT", 80))
}

/// SIGTERM/SIGINT/SIGHUP: stop every process, then quit. Exits anyway if
/// the UI doesn't quit within a few seconds.
async fn stop_on_signal(core: Core, commands: mpsc::UnboundedSender<UiCommand>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        tracing::error!("can't install signal handlers");
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        _ = hup.recv() => {}
    }
    tracing::info!("signal received; stopping everything");
    core.shutdown().await;
    let _ = commands.send(UiCommand::Exit);
    tokio::time::sleep(Duration::from_secs(3)).await;
    std::process::exit(0);
}

fn init_actions(cx: &mut App) {
    // Bind keys before building menus so the menus show the shortcuts.
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-n", NewCluster, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new(
            "up",
            sidebar::SelectPreviousCluster,
            Some(sidebar::CLUSTER_LIST_CONTEXT),
        ),
        KeyBinding::new(
            "down",
            sidebar::SelectNextCluster,
            Some(sidebar::CLUSTER_LIST_CONTEXT),
        ),
    ]);
    cx.on_action(|_: &Quit, cx| quit::request_quit(cx));
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &ShowWindow, cx| show_main_window(cx));
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(handle) = MainWindow::handle(cx) {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.on_action(|_: &NewCluster, cx| {
        show_main_window(cx);
        MainWindow::with_workspace(cx, |ws, window, cx| ws.open_new_cluster(window, cx));
    });
    cx.on_action(|_: &OpenSettings, cx| {
        show_main_window(cx);
        MainWindow::with_workspace(cx, |ws, window, cx| ws.open_settings(window, cx));
    });

    use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    cx.set_menus([
        Menu::new("Grove").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Hide Grove", Hide),
            MenuItem::action("Quit Grove", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Cluster…", NewCluster),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Window").items([MenuItem::action("Show Grove", ShowWindow)]),
    ]);
}

fn pump_commands(cx: &mut App, mut commands: mpsc::UnboundedReceiver<UiCommand>) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(command) = commands.recv().await {
            cx.update(|cx| match command {
                UiCommand::ShowWindow => show_main_window(cx),
                UiCommand::Exit => exit_now(cx),
                UiCommand::Tray(id) => tray::handle(&id, cx),
            });
        }
    })
    .detach();
}

/// Opens the main window, or brings it forward.
pub fn show_main_window(cx: &mut App) {
    platform::set_dock_icon_visible(true);
    if let Some(handle) = MainWindow::handle(cx)
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return;
    }

    let store = store::AppStore::entity(cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1180.), px(780.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some("dev.grove.Grove".into()),
        // Unified title bar: the sidebar and the content toolbar run to the
        // top edge, with the traffic lights sitting in the sidebar.
        titlebar: Some(TitlebarOptions {
            title: Some("Grove".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(
                ui::TRAFFIC_LIGHTS_INSET,
                (ui::TOOLBAR_HEIGHT - ui::TRAFFIC_LIGHT_SIZE) / 2.,
            )),
        }),
        // Grove draws its own drag areas (ui::WindowDrag), so AppKit must not
        // treat the top strip as a system title bar that swallows clicks.
        app_owns_titlebar_drag: true,
        // Behind-window blur for the translucent sidebar.
        window_background: WindowBackgroundAppearance::Blurred,
        ..WindowOptions::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(store, window, cx));
        cx.global_mut::<MainWindow>().workspace = Some(workspace.downgrade());
        // Root paints the theme background over the whole window by default,
        // which would hide the blur; the workspace paints its own surfaces.
        cx.new(|cx| Root::new(workspace, window, cx).bg(gpui_kit::transparent_black()))
    });
    match opened {
        Ok(handle) => {
            cx.global_mut::<MainWindow>().handle = Some(handle.into());
            cx.activate(true);
        }
        Err(e) => tracing::error!("can't open the window: {e:#}"),
    }
}

fn watch_window_close(cx: &mut App) {
    cx.on_window_closed(|cx, id| {
        let main = cx.global_mut::<MainWindow>();
        if main.handle.is_some_and(|h| h.window_id() == id) {
            main.handle = None;
            main.workspace = None;
            platform::set_dock_icon_visible(false);
        }
    })
    .detach();
}

/// Safety net for quits that bypass [`quit::request_quit`] (e.g. logging
/// out): stop every process before the app exits. Blocks the main thread,
/// which is fine at that point.
fn stop_core_on_quit(cx: &mut App) {
    cx.on_app_quit(|cx| {
        let services = Services::get(cx);
        services.rt.block_on(services.core.shutdown());
        drop_ipc_server(cx);
        async {}
    })
    .detach();
}

/// Removes the IPC socket (the server handle's drop does).
pub fn drop_ipc_server(cx: &mut App) {
    if cx.has_global::<IpcServer>() {
        cx.global_mut::<IpcServer>().0.take();
    }
}

/// Quits without asking; the core is already stopped.
pub fn exit_now(cx: &mut App) {
    drop_ipc_server(cx);
    cx.quit();
}

/// Starts a cluster from anywhere (menu bar, onboarding).
pub fn start_cluster(cx: &mut App, name: String) {
    run_op(
        cx,
        format!("Couldn't start {name}"),
        move |core| async move { core.start_cluster(&name).await },
    )
    .detach();
}
