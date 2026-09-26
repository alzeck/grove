//! The menu bar item: clusters with their state, start/stop, URLs, and
//! Show/Quit.

use super::app::{UiCommand, show_main_window, start_cluster};
use super::quit;
use super::services::{Services, run_op};
use super::store::AppStore;
use gpui_kit::App;
use grove_core::Snapshot;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(target_os = "macos")]
pub fn init(cx: &mut App, commands: UnboundedSender<UiCommand>) {
    use gpui_kit::Global;
    use tray_icon::menu::MenuEvent;
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    struct Tray {
        icon: TrayIcon,
        signature: String,
    }
    impl Global for Tray {}

    fn sync(cx: &mut App) {
        let snapshot = AppStore::snapshot(cx);
        let signature = signature(&snapshot);
        let Some(tray) = cx.try_global::<Tray>() else {
            return;
        };
        if tray.signature == signature {
            return;
        }
        tray.icon.set_menu(Some(Box::new(build_menu(&snapshot))));
        cx.global_mut::<Tray>().signature = signature;
    }

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = commands.send(UiCommand::Tray(event.id.0));
    }));

    let (rgba, width, height) = glyph();
    let icon = match Icon::from_rgba(rgba, width, height) {
        Ok(icon) => icon,
        Err(e) => {
            tracing::error!("menu bar icon: {e}");
            return;
        }
    };
    let snapshot = AppStore::snapshot(cx);
    let built = TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_tooltip("Grove")
        .with_menu(Box::new(build_menu(&snapshot)))
        .build();
    match built {
        Ok(icon) => {
            cx.set_global(Tray {
                icon,
                signature: signature(&snapshot),
            });
            let store = AppStore::entity(cx);
            cx.observe(&store, |_, cx| sync(cx)).detach();
        }
        Err(e) => tracing::error!("can't create the menu bar item: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn init(_: &mut App, _: UnboundedSender<UiCommand>) {}

/// What the menu shows; the menu is rebuilt only when this changes.
fn signature(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    for c in &snapshot.clusters {
        out.push_str(&c.name);
        out.push('\u{1}');
        out.push_str(c.phase.label());
        for (label, url) in c.urls() {
            out.push('\u{2}');
            out.push_str(&label);
            out.push_str(&url);
        }
        out.push('\n');
    }
    out
}

#[cfg(target_os = "macos")]
fn build_menu(snapshot: &Snapshot) -> tray_icon::menu::Menu {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let menu = Menu::new();
    let running = snapshot
        .clusters
        .iter()
        .filter(|c| c.phase.is_active())
        .count();
    let header = match (snapshot.clusters.len(), running) {
        (0, _) => "No clusters".to_string(),
        (_, 0) => "Nothing running".to_string(),
        (_, 1) => "1 cluster running".to_string(),
        (_, n) => format!("{n} clusters running"),
    };
    let _ = menu.append(&MenuItem::new(header, false, None));
    let _ = menu.append(&PredefinedMenuItem::separator());

    for c in &snapshot.clusters {
        let dot = if c.phase.is_active() { "●" } else { "○" };
        let sub = Submenu::new(format!("{dot}  {} — {}", c.name, c.phase.label()), true);
        let busy = matches!(
            c.phase,
            grove_core::ClusterPhase::Creating { .. }
                | grove_core::ClusterPhase::TearingDown
                | grove_core::ClusterPhase::Error { .. }
        );
        let _ = sub.append(&MenuItem::with_id(
            format!("start:{}", c.name),
            "Start",
            !busy && !c.phase.is_active(),
            None,
        ));
        let _ = sub.append(&MenuItem::with_id(
            format!("stop:{}", c.name),
            "Stop",
            c.phase.is_active(),
            None,
        ));
        let urls = c.urls();
        if !urls.is_empty() {
            let _ = sub.append(&PredefinedMenuItem::separator());
            for (label, url) in urls {
                let _ = sub.append(&MenuItem::with_id(
                    format!("open:{url}"),
                    format!("Open {label}  {url}"),
                    true,
                    None,
                ));
            }
        }
        let _ = menu.append(&sub);
    }
    if !snapshot.clusters.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    let _ = menu.append(&MenuItem::with_id("show", "Show Grove", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quit Grove", true, None));
    menu
}

/// Handles a chosen menu bar item.
pub fn handle(id: &str, cx: &mut App) {
    match id {
        "show" => show_main_window(cx),
        "quit" => quit::request_quit(cx),
        _ => {
            if let Some(name) = id.strip_prefix("start:") {
                start_cluster(cx, name.to_string());
            } else if let Some(name) = id.strip_prefix("stop:") {
                let name = name.to_string();
                run_op(
                    cx,
                    format!("Couldn't stop {name}"),
                    move |core| async move { core.stop_cluster(&name).await },
                )
                .detach();
            } else if let Some(url) = id.strip_prefix("open:") {
                let core = Services::get(cx).core;
                if let Err(e) = core.open_url(url) {
                    tracing::warn!("can't open {url}: {e}");
                }
            }
        }
    }
}

/// The menu bar template glyph (three pines) as RGBA, 36x36 (18pt at 2x).
/// Rendered from `assets/tray.svg`; see `assets/icon.py`.
pub(super) fn glyph() -> (Vec<u8>, u32, u32) {
    const SIZE: u32 = 36;
    let rgba = include_bytes!("../../assets/tray-template.rgba");
    debug_assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
    (rgba.to_vec(), SIZE, SIZE)
}
