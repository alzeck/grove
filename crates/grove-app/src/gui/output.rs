//! The output panel: tabs of terminals showing process output, setup steps
//! and interactive shells.

use super::services::{Services, notify, run_op};
use super::store::Store;
use bytes::Bytes;
use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, AsyncWindowContext, Context, Entity, Focusable as _, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, div,
};
use grove_core::{ManagedProcess, NoticeLevel};
use grove_proc::ProcStatus;
use grove_term::{PtyHandle, Rgb, TerminalFeed, TerminalSettings, TerminalView};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;

/// How much of a log file to show for a process that isn't running.
const LOG_TAIL_BYTES: u64 = 256 * 1024;
const SHELL_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, PartialEq, Eq)]
enum TabKind {
    Process {
        cluster: String,
        project: String,
        process: String,
    },
    Activity {
        cluster: String,
        id: u64,
    },
    Shell {
        cluster: String,
        project: String,
    },
}

struct OutputTab {
    id: u64,
    kind: TabKind,
    title: SharedString,
    view: Entity<TerminalView>,
    /// The process shown; `None` for a placeholder or log tail.
    process: Option<ManagedProcess>,
    exited: bool,
}

pub struct OutputPanel {
    tabs: Vec<OutputTab>,
    active: Option<u64>,
    next_id: u64,
    rt: tokio::runtime::Handle,
    _store: Subscription,
    _theme: Subscription,
}

impl OutputPanel {
    pub fn new(store: &Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_id: 1,
            rt: Services::get(cx).rt,
            _store: cx.observe(store, |this, _, cx| this.sync(cx)),
            // Terminals sit on the app background; follow light/dark switches.
            _theme: cx.observe_global::<Theme>(|this, cx| this.restyle(cx)),
        }
    }

    fn restyle(&mut self, cx: &mut Context<Self>) {
        let settings = terminal_settings(cx);
        for tab in &self.tabs {
            if tab.view.read(cx).settings() != &settings {
                let settings = settings.clone();
                tab.view
                    .update(cx, |view, cx| view.set_settings(settings, cx));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Shows a process's output, following it across restarts.
    pub fn open_process(
        &mut self,
        cluster: &str,
        project: &str,
        process: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = TabKind::Process {
            cluster: cluster.into(),
            project: project.into(),
            process: process.into(),
        };
        if self.activate(&kind, window, cx) {
            return;
        }
        let services = Services::get(cx);
        let handle = services.core.process_handle(cluster, project, process);
        let (feed, pty) = match &handle {
            Some(p) => (live_feed(p), pty_for(p)),
            None => {
                let log = services.core.home().log_path(cluster, project, process);
                (log_feed(&log), Arc::new(NullPty) as Arc<dyn PtyHandle>)
            }
        };
        let title = if cluster == grove_config::DEFAULT_CLUSTER {
            format!("{project}.{process}")
        } else {
            format!("{project}.{process} · {cluster}")
        };
        self.push(kind, title, handle, feed, pty, window, cx);
    }

    /// Shows the output of a setup/git step, if it ran a command.
    pub fn open_activity(
        &mut self,
        cluster: &str,
        id: u64,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = TabKind::Activity {
            cluster: cluster.into(),
            id,
        };
        if self.activate(&kind, window, cx) {
            return;
        }
        let Some(handle) = Services::get(cx).core.activity_handle(cluster, id) else {
            notify(cx, NoticeLevel::Info, "This step has no command output.");
            return;
        };
        let (feed, pty) = (live_feed(&handle), pty_for(&handle));
        self.push(kind, title.to_string(), Some(handle), feed, pty, window, cx);
    }

    /// Opens an interactive shell in the project's checkout.
    pub fn open_shell(
        &mut self,
        cluster: &str,
        project: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (c, p) = (cluster.to_string(), project.to_string());
        let task = run_op(cx, format!("Couldn't open a shell in {project}"), {
            let (c, p) = (c.clone(), p.clone());
            move |core| async move { core.open_shell(&c, &p).await }
        });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let Some(shell) = task.await else { return };
            let _ = this.update_in(cx, |panel, window, cx| {
                let kind = TabKind::Shell {
                    cluster: c.clone(),
                    project: p.clone(),
                };
                let title = if c == grove_config::DEFAULT_CLUSTER {
                    format!("Shell · {p}")
                } else {
                    format!("Shell · {p} · {c}")
                };
                let (feed, pty) = (live_feed(&shell), pty_for(&shell));
                let id = panel.push(kind, title, Some(shell.clone()), feed, pty, window, cx);
                panel.watch_exit(id, shell, cx);
            });
        })
        .detach();
    }

    fn activate(&mut self, kind: &TabKind, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(id) = self
            .tabs
            .iter()
            .find(|t| &t.kind == kind && !matches!(t.kind, TabKind::Shell { .. }))
            .map(|t| t.id)
        else {
            return false;
        };
        self.select(id, window, cx);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: TabKind,
        title: String,
        process: Option<ManagedProcess>,
        feed: TerminalFeed,
        pty: Arc<dyn PtyHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let exited = process
            .as_ref()
            .is_none_or(|p| matches!(p.status(), ProcStatus::Exited(_)));
        let settings = terminal_settings(cx);
        let view = cx.new(|cx| {
            let mut view = TerminalView::with_settings(feed, pty, settings, window, cx);
            view.set_read_only(exited, cx);
            view
        });
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(OutputTab {
            id,
            kind,
            title: title.into(),
            view,
            process,
            exited,
        });
        self.select(id, window, cx);
        id
    }

    fn select(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.active = Some(id);
        if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
            let focus = tab.view.read(cx).focus_handle(cx);
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn close(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let tab = self.tabs.remove(ix);
        if let (TabKind::Shell { .. }, Some(shell)) = (&tab.kind, tab.process) {
            self.rt.spawn(async move {
                shell.terminate(SHELL_GRACE).await;
            });
        }
        if self.active == Some(id) {
            let next = self.tabs.get(ix).or_else(|| self.tabs.last()).map(|t| t.id);
            match next {
                Some(next) => self.select(next, window, cx),
                None => self.active = None,
            }
        }
        cx.notify();
    }

    fn clear_active(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            tab.view.update(cx, |view, cx| view.clear(cx));
        }
    }

    fn active_tab(&self) -> Option<&OutputTab> {
        let id = self.active?;
        self.tabs.iter().find(|t| t.id == id)
    }

    /// Marks a shell's tab once the shell exits.
    fn watch_exit(&mut self, id: u64, shell: ManagedProcess, cx: &mut Context<Self>) {
        let wait = self.rt.spawn(async move { shell.wait().await });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let _ = wait.await;
            let _ = this.update(cx, |panel, cx| {
                if let Some(tab) = panel.tabs.iter_mut().find(|t| t.id == id) {
                    tab.exited = true;
                    tab.view.update(cx, |view, cx| view.set_read_only(true, cx));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Follows restarts: a process tab whose process changed gets the new
    /// process's output; exited processes become read-only.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let core = Services::get(cx).core;
        let mut changed = false;
        for tab in &mut self.tabs {
            let current = match &tab.kind {
                TabKind::Process {
                    cluster,
                    project,
                    process,
                } => core.process_handle(cluster, project, process),
                TabKind::Activity { .. } | TabKind::Shell { .. } => tab.process.clone(),
            };
            let same = match (&current, &tab.process) {
                (Some(a), Some(b)) => a.pid() == b.pid(),
                (None, None) => true,
                _ => false,
            };
            if !same && let Some(process) = &current {
                let (feed, pty) = (live_feed(process), pty_for(process));
                tab.view.update(cx, |view, cx| {
                    view.replace_feed(feed, pty, cx);
                    view.set_read_only(false, cx);
                });
                tab.process = current.clone();
                tab.exited = false;
                changed = true;
            }
            let exited = current
                .as_ref()
                .is_none_or(|p| matches!(p.status(), ProcStatus::Exited(_)));
            if exited != tab.exited {
                tab.exited = exited;
                tab.view
                    .update(cx, |view, cx| view.set_read_only(exited, cx));
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }
}

impl Drop for OutputPanel {
    /// Shells belong to their tabs; the window closing ends them.
    fn drop(&mut self) {
        for tab in self.tabs.drain(..) {
            if let (TabKind::Shell { .. }, Some(shell)) = (tab.kind, tab.process) {
                self.rt.spawn(async move {
                    shell.terminate(SHELL_GRACE).await;
                });
            }
        }
    }
}

impl Render for OutputPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active_ix = self
            .active
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let tabs = self.tabs.iter().map(|tab| {
            let id = tab.id;
            let color = if tab.exited {
                theme.muted_foreground.opacity(0.6)
            } else {
                theme.success
            };
            Tab::new()
                .label(tab.title.clone())
                .prefix(div().pl_2().child(super::ui::dot(color)))
                .suffix(
                    super::ui::icon_button(
                        ("close-tab", id),
                        IconName::X,
                        if matches!(tab.kind, TabKind::Shell { .. }) {
                            "Close and end the shell"
                        } else {
                            "Close"
                        },
                        false,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close(id, window, cx);
                    })),
                )
        });
        let ids: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.tab_bar)
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            TabBar::new("output-tabs")
                                .children(tabs)
                                .selected_index(active_ix)
                                .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                                    if let Some(id) = ids.get(*ix) {
                                        this.select(*id, window, cx);
                                    }
                                })),
                        ),
                    )
                    .child(
                        h_flex().flex_none().px_2().gap_1().child(
                            super::ui::icon_button(
                                "clear-output",
                                IconName::Eraser,
                                "Clear",
                                false,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.clear_active(cx))),
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when_some(self.active_tab(), |this, tab| this.child(tab.view.clone())),
            )
    }
}

/// Replay plus live output; lagging just skips ahead.
fn live_feed(process: &ManagedProcess) -> TerminalFeed {
    let sub = process.subscribe();
    let live = BroadcastStream::new(sub.rx)
        .filter_map(|item| futures::future::ready(item.ok()))
        .boxed();
    TerminalFeed {
        replay: sub.replay,
        live,
    }
}

/// The end of a process's log, for a process that isn't running.
fn log_feed(path: &std::path::Path) -> TerminalFeed {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut replay = Vec::new();
    if let Ok(mut file) = std::fs::File::open(path) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len > LOG_TAIL_BYTES {
            let _ = file.seek(SeekFrom::Start(len - LOG_TAIL_BYTES));
        }
        let _ = file.read_to_end(&mut replay);
    }
    replay.extend_from_slice(b"\r\n\x1b[2m-- not running --\x1b[0m\r\n");
    TerminalFeed {
        replay: Bytes::from(replay),
        live: futures::stream::empty().boxed(),
    }
}

/// Keyboard input goes through a thread: writing to a PTY whose program
/// isn't reading can block.
struct ProcessPty {
    input: std::sync::mpsc::Sender<Vec<u8>>,
    process: ManagedProcess,
}

impl PtyHandle for ProcessPty {
    fn write(&self, data: &[u8]) {
        let _ = self.input.send(data.to_vec());
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.process.resize(cols, rows);
    }
}

fn pty_for(process: &ManagedProcess) -> Arc<dyn PtyHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let writer = process.clone();
    let spawned = std::thread::Builder::new()
        .name("grove-pty-input".into())
        .spawn(move || {
            while let Ok(data) = rx.recv() {
                if writer.write(&data).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("can't start terminal input thread: {e}");
    }
    Arc::new(ProcessPty {
        input: tx,
        process: process.clone(),
    })
}

struct NullPty;

impl PtyHandle for NullPty {
    fn write(&self, _: &[u8]) {}
    fn resize(&self, _: u16, _: u16) {}
}

/// Terminal colours that sit on the app's own background.
fn terminal_settings(cx: &App) -> TerminalSettings {
    let theme = cx.theme();
    let rgba = theme.background.to_rgb();
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let background = Rgb {
        r: channel(rgba.r),
        g: channel(rgba.g),
        b: channel(rgba.b),
    };
    let mut settings = TerminalSettings::default();
    if theme.is_dark() {
        settings.dark_theme.background = background;
    } else {
        settings.light_theme.background = background;
    }
    settings
}
