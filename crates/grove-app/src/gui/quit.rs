//! Quitting: confirm if anything runs, stop everything, then exit.

use super::app::{exit_now, show_main_window};
use super::services::{MainWindow, Services};
use super::store::AppStore;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, AsyncApp, Global};
use grove_core::ClusterPhase;

#[derive(Default)]
struct Quitting(bool);

impl Global for Quitting {}

/// Cmd+Q, "Quit Grove" in the menus: asks first when clusters are running.
pub fn request_quit(cx: &mut App) {
    if cx.try_global::<Quitting>().is_some_and(|q| q.0) {
        return;
    }
    let snapshot = AppStore::snapshot(cx);
    let running: Vec<String> = snapshot
        .clusters
        .iter()
        .filter(|c| {
            c.phase.is_active()
                || matches!(
                    c.phase,
                    ClusterPhase::Creating { .. }
                        | ClusterPhase::Stopping
                        | ClusterPhase::TearingDown
                )
        })
        .map(|c| c.name.clone())
        .collect();
    if running.is_empty() {
        quit_now(cx);
        return;
    }

    show_main_window(cx);
    let Some(handle) = MainWindow::handle(cx) else {
        quit_now(cx);
        return;
    };
    let description = match running.as_slice() {
        [one] => format!("“{one}” is running. Its processes will be stopped."),
        many => format!(
            "{} clusters are running ({}). Their processes will be stopped.",
            many.len(),
            many.join(", ")
        ),
    };
    let _ = handle.update(cx, move |_, window, cx| {
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .title("Quit Grove?")
                .description(description.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Quit")
                        .ok_variant(ButtonVariant::Danger)
                        .show_cancel(true),
                )
                .on_ok(|_, _, cx| {
                    quit_now(cx);
                    true
                })
        });
    });
}

/// Stops every process and the proxy, then quits.
pub fn quit_now(cx: &mut App) {
    if cx.try_global::<Quitting>().is_some_and(|q| q.0) {
        return;
    }
    cx.set_global(Quitting(true));
    if let Some(handle) = MainWindow::handle(cx) {
        let _ = handle.update(cx, |_, window, cx| {
            window.push_notification(Notification::info("Stopping everything…"), cx);
        });
    }
    let services = Services::get(cx);
    let job = services
        .rt
        .spawn(async move { services.core.shutdown().await });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let _ = job.await;
        cx.update(exit_now);
    })
    .detach();
}
