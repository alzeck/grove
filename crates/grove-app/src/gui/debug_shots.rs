//! Dev aid, only with `--features debug-screenshots`: with
//! `GROVE_DEBUG_SHOTS=<dir>`, walks the main window through a few states
//! (running cluster, output, new-cluster dialog, settings, both
//! appearances), renders each to `<dir>/<n>-<state>.png`, then quits.
//!
//! GPUI renders the window's own pixels only: the desktop blur behind the
//! translucent sidebar and the native traffic lights aren't in the
//! captures. Transparent areas are composited over a stand-in wallpaper.

use super::app::show_main_window;
use super::quit::quit_now;
use super::services::{MainWindow, run_op};
use super::theme;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, AsyncApp};
use grove_config::Appearance;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn start(cx: &mut App) {
    let Ok(dir) = std::env::var("GROVE_DEBUG_SHOTS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    let (rgba, width, height) = super::tray::glyph();
    if let Some(icon) = image::RgbaImage::from_raw(width, height, rgba) {
        let _ = icon.save(dir.join("tray-icon.png"));
    }
    show_main_window(cx);
    cx.spawn(async move |cx: &mut AsyncApp| {
        let executor = cx.background_executor().clone();
        let wait = move |ms: u64| executor.timer(Duration::from_millis(ms));
        cx.update(|cx| theme::set_appearance(Appearance::Dark, cx));
        wait(2500).await;
        shot(cx, &dir, "01-main").await;
        let configured = cx.update(|cx| {
            matches!(
                super::store::AppStore::snapshot(cx).config,
                grove_core::ConfigStatus::Loaded { .. }
            )
        });
        if !configured {
            cx.update(|cx| theme::set_appearance(Appearance::Light, cx));
            wait(800).await;
            shot(cx, &dir, "01-main-light").await;
            cx.update(quit_now);
            return;
        }

        cx.update(|cx| {
            run_op(cx, "debug start", |core| async move {
                core.start_cluster("default").await
            })
            .detach()
        });
        wait(4000).await;
        shot(cx, &dir, "02-running").await;

        cx.update(|cx| {
            MainWindow::with_workspace(cx, |ws, window, cx| {
                ws.output.update(cx, |panel, cx| {
                    panel.open_process("default", "api", "server", window, cx)
                });
            })
        });
        wait(1500).await;
        shot(cx, &dir, "03-output").await;

        cx.update(|cx| {
            MainWindow::with_workspace(cx, |ws, window, cx| {
                ws.output.update(cx, |panel, cx| {
                    panel.open_shell("default", "web", window, cx)
                });
            })
        });
        wait(2500).await;
        shot(cx, &dir, "03b-shell").await;

        // A stopped process shows the end of its log.
        let stop = cx.update(|cx| {
            run_op(cx, "debug stop", |core| async move {
                core.stop_process("default", "web", "app").await
            })
        });
        let _ = stop.await;
        cx.update(|cx| {
            MainWindow::with_workspace(cx, |ws, window, cx| {
                ws.output.update(cx, |panel, cx| {
                    panel.open_process("default", "web", "app", window, cx)
                });
            })
        });
        wait(1000).await;
        shot(cx, &dir, "03c-stopped-output").await;

        let dialog = cx.update(|cx| {
            let handle = MainWindow::handle(cx)?;
            let workspace = cx.global::<MainWindow>().workspace.clone()?;
            handle
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |ws, cx| ws.open_new_cluster_dialog(window, cx))
                        .ok()
                        .flatten()
                })
                .ok()
                .flatten()
        });
        wait(1000).await;
        shot(cx, &dir, "04-new-cluster").await;
        if let Some(dialog) = &dialog {
            cx.update(|cx| {
                if let Some(handle) = MainWindow::handle(cx) {
                    let _ = handle.update(cx, |_, window, cx| {
                        dialog.update(cx, |d, cx| d.debug_plan_branch("feature/both", window, cx));
                    });
                }
            });
            wait(4000).await;
            shot(cx, &dir, "05-plan").await;
            cx.update(|cx| {
                if let Some(handle) = MainWindow::handle(cx) {
                    let _ = handle.update(cx, |_, window, cx| {
                        dialog.update(cx, |d, cx| d.debug_create(window, cx));
                    });
                }
            });
            wait(6000).await;
            shot(cx, &dir, "06-created").await;
        }

        cx.update(|cx| {
            MainWindow::with_workspace(cx, |ws, window, cx| {
                ws.open_teardown("feature-both".into(), window, cx)
            })
        });
        wait(2500).await;
        shot(cx, &dir, "07-teardown").await;
        close_overlays(cx);

        cx.update(|cx| {
            MainWindow::with_workspace(cx, |ws, window, cx| ws.open_settings(window, cx))
        });
        wait(3000).await;
        shot(cx, &dir, "08-settings").await;
        close_overlays(cx);

        // The same screens in both appearances.
        for (appearance, suffix) in [(Appearance::Dark, "dark"), (Appearance::Light, "light")] {
            cx.update(|cx| theme::set_appearance(appearance, cx));
            wait(1000).await;
            shot(cx, &dir, &format!("20-main-{suffix}")).await;
            cx.update(|cx| {
                MainWindow::with_workspace(cx, |ws, window, cx| ws.open_settings(window, cx))
            });
            wait(2500).await;
            shot(cx, &dir, &format!("21-settings-{suffix}")).await;
            close_overlays(cx);
            cx.update(|cx| {
                MainWindow::with_workspace(cx, |ws, window, cx| ws.open_new_cluster(window, cx))
            });
            wait(1000).await;
            shot(cx, &dir, &format!("22-new-cluster-{suffix}")).await;
            close_overlays(cx);
            cx.update(|cx| {
                MainWindow::with_workspace(cx, |ws, window, cx| {
                    ws.open_teardown("feature-both".into(), window, cx)
                })
            });
            wait(2000).await;
            shot(cx, &dir, &format!("23-teardown-{suffix}")).await;
            close_overlays(cx);
        }

        cx.update(super::quit::request_quit);
        wait(500).await;
        shot(cx, &dir, "11-quit").await;
        cx.update(quit_now);
    })
    .detach();
}

fn close_overlays(cx: &mut AsyncApp) {
    cx.update(|cx| {
        if let Some(handle) = MainWindow::handle(cx) {
            let _ = handle.update(cx, |_, window, cx| {
                window.close_all_dialogs(cx);
                window.close_sheet(cx);
            });
        }
    });
}

async fn shot(cx: &mut AsyncApp, dir: &Path, name: &str) {
    // Let GPUI draw and present on its own (overlays animate in from their
    // first frame), then capture.
    cx.update(|cx| {
        if let Some(handle) = MainWindow::handle(cx) {
            let _ = handle.update(cx, |_, window, _| {
                window.activate_window();
                window.refresh();
            });
        }
    });
    cx.background_executor()
        .timer(Duration::from_millis(900))
        .await;
    cx.update(|cx| {
        let Some(handle) = MainWindow::handle(cx) else {
            return;
        };
        // The window as the window server shows it (glass, traffic lights),
        // when it's on screen.
        #[cfg(target_os = "macos")]
        if let Ok(Some(number)) = handle.update(cx, |_, window, _| {
            super::platform::SidebarGlass::window_number(window)
        }) {
            let path = dir.join(format!("{name}-window.png"));
            let _ = std::process::Command::new("screencapture")
                .arg("-x")
                .arg("-o")
                .arg(format!("-l{number}"))
                .arg(&path)
                .status();
        }
        // `render_to_image` takes a drawable from the window's Metal layer
        // and never presents it, which stalls what the window server shows;
        // `GROVE_DEBUG_SHOTS_WINDOW_ONLY` skips it for faithful window
        // captures.
        if std::env::var_os("GROVE_DEBUG_SHOTS_WINDOW_ONLY").is_some() {
            return;
        }
        let rendered = handle.update(cx, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.render_to_image()
        });
        match rendered {
            Ok(Ok(mut image)) => {
                over_wallpaper(&mut image);
                let path = dir.join(format!("{name}.png"));
                if let Err(e) = image.save(&path) {
                    tracing::error!("saving {}: {e}", path.display());
                }
            }
            Ok(Err(e)) => tracing::error!("rendering {name}: {e:#}"),
            Err(e) => tracing::error!("rendering {name}: {e:#}"),
        }
    });
}

/// Composites the (premultiplied) window pixels over a soft two-tone
/// gradient standing in for the blurred desktop.
fn over_wallpaper(image: &mut image::RgbaImage) {
    let (w, h) = image.dimensions();
    let top = [70.0f32, 96.0, 150.0];
    let bottom = [150.0f32, 104.0, 128.0];
    for (x, y, px) in image.enumerate_pixels_mut() {
        let t = (x as f32 / w as f32 * 0.4 + y as f32 / h as f32 * 0.6).clamp(0.0, 1.0);
        let a = px[3] as f32 / 255.0;
        for c in 0..3 {
            let bg = top[c] + (bottom[c] - top[c]) * t;
            px[c] = (px[c] as f32 + bg * (1.0 - a)).round().min(255.0) as u8;
        }
        px[3] = 255;
    }
}
