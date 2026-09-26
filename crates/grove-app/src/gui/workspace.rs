//! The main window: sidebar of clusters, the selected cluster, the output
//! panel; onboarding while Grove isn't configured.

use super::new_cluster::NewClusterDialog;
use super::onboarding::{Onboarding, OnboardingEvent};
use super::output::OutputPanel;
use super::services::{Services, notify, run_op};
use super::settings::SettingsView;
use super::store::Store;
use super::teardown::TeardownDialog;
use super::{platform, theme, ui};
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::{ResizableState, resizable_panel, v_resizable};
use gpui_kit::component::{ActiveTheme as _, Root, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Entity, FocusHandle, IntoElement, Render, SharedString, Subscription, Task,
    WeakEntity, Window, div, px,
};
use grove_core::{ConfigStatus, Core, NoticeLevel, Snapshot};
use std::collections::HashSet;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const GIT_REFRESH: Duration = Duration::from_secs(10);

pub struct Workspace {
    pub(super) store: Entity<Store>,
    /// Name of the selected cluster.
    pub(super) selected: String,
    pub(super) output: Entity<OutputPanel>,
    split: Entity<ResizableState>,
    onboarding: Option<Entity<Onboarding>>,
    /// Operations in flight, keyed like `start:<cluster>`, for busy buttons.
    pub(super) pending: HashSet<String>,
    pub(super) sidebar_drag: ui::WindowDrag,
    /// Keyboard focus of the sidebar's cluster list (arrow keys).
    pub(super) cluster_list_focus: FocusHandle,
    /// The Liquid Glass pane behind the sidebar, when AppKit has one.
    pub(super) glass: Option<Rc<platform::SidebarGlass>>,
    toolbar_drag: ui::WindowDrag,
    _subscriptions: Vec<Subscription>,
    _git_refresh: Task<()>,
}

impl Workspace {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        theme::sync(Some(window), cx);
        let output = cx.new(|cx| OutputPanel::new(&store, cx));
        // The window keeps its behind-window blur under the glass: it gives
        // the glass an appearance-aware base, so the sidebar stays light in
        // light mode even over a dark wallpaper (GPUI's text isn't vibrant
        // and can't adapt the way AppKit's does).
        let glass = platform::SidebarGlass::install(window).map(Rc::new);
        let cluster_list_focus = cx.focus_handle();
        let subscriptions = vec![
            // The selection pill turns accent-filled while the list has focus.
            cx.on_focus(&cluster_list_focus, window, |_, _, cx| cx.notify()),
            cx.on_blur(&cluster_list_focus, window, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                theme::sync(Some(window), cx);
            }),
            cx.observe(&output, |_, _, cx| cx.notify()),
            // Sidebar icons are coloured only while the window is key.
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.on_snapshot(window, cx);
                cx.notify();
            }),
        ];
        let mut this = Self {
            store,
            selected: grove_config::DEFAULT_CLUSTER.to_string(),
            output,
            split: cx.new(|_| ResizableState::default()),
            onboarding: None,
            pending: HashSet::new(),
            sidebar_drag: ui::WindowDrag::default(),
            cluster_list_focus,
            glass,
            toolbar_drag: ui::WindowDrag::default(),
            _subscriptions: subscriptions,
            _git_refresh: Task::ready(()),
        };
        this.on_snapshot(window, cx);
        this._git_refresh = this.start_git_refresh(cx);
        this.select_cluster(this.selected.clone(), cx);
        this
    }

    pub(super) fn snapshot(&self, cx: &gpui_kit::App) -> Arc<Snapshot> {
        self.store.read(cx).snapshot()
    }

    /// Shows onboarding when Grove isn't configured. It stays until the user
    /// finishes it, even though the config loads halfway through.
    fn on_snapshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(cx);
        let needs_setup = matches!(
            snapshot.config,
            ConfigStatus::NotConfigured | ConfigStatus::WorkspaceMissing { .. }
        );
        if needs_setup && self.onboarding.is_none() {
            let onboarding = cx.new(|cx| Onboarding::new(window, cx));
            self._subscriptions.push(cx.subscribe_in(
                &onboarding,
                window,
                |this, _, event: &OnboardingEvent, _, cx| match event {
                    OnboardingEvent::Done => {
                        this.onboarding = None;
                        cx.notify();
                    }
                },
            ));
            self.onboarding = Some(onboarding);
        }
    }

    pub(super) fn select_cluster(&mut self, name: String, cx: &mut Context<Self>) {
        self.selected = name.clone();
        let has_prs = self
            .snapshot(cx)
            .cluster(&name)
            .is_some_and(|c| c.instances.iter().any(|i| i.pr.is_some()));
        let core = Services::get(cx).core;
        let rt = Services::get(cx).rt;
        rt.spawn(async move {
            core.refresh_git_status(&name).await;
            if has_prs && let Err(e) = core.refresh_prs(&name).await {
                tracing::debug!("refreshing PRs of {name}: {e}");
            }
        });
        cx.notify();
    }

    /// Refreshes the selected cluster's git status while the window is open.
    fn start_git_refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            loop {
                cx.background_executor().timer(GIT_REFRESH).await;
                let Ok(name) = this.update(cx, |ws, _| ws.selected.clone()) else {
                    break;
                };
                let services = cx.update(|cx| Services::get(cx));
                services.rt.spawn(async move {
                    services.core.refresh_git_status(&name).await;
                });
            }
        })
    }

    /// Runs a core operation with `key` marked busy until it finishes.
    pub(super) fn run_tracked<T, Fut>(
        &mut self,
        key: String,
        what: impl Into<SharedString>,
        op: impl FnOnce(Core) -> Fut,
        then: impl FnOnce(&mut Self, Option<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        Fut: Future<Output = grove_core::Result<T>> + Send + 'static,
    {
        if !self.pending.insert(key.clone()) {
            return;
        }
        cx.notify();
        let task = run_op(cx, what, op);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.pending.remove(&key);
                then(ws, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn is_pending(&self, key: &str) -> bool {
        self.pending.contains(key)
    }

    pub fn open_new_cluster(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_cluster_dialog(window, cx);
    }

    pub(super) fn open_new_cluster_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<NewClusterDialog>> {
        if !matches!(self.snapshot(cx).config, ConfigStatus::Loaded { .. }) {
            notify(
                cx,
                NoticeLevel::Warning,
                "Finish setting up Grove before creating clusters.",
            );
            return None;
        }
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| NewClusterDialog::new(workspace, window, cx));
        let dialog = view.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            NewClusterDialog::build(&view, dialog, window, cx)
        });
        Some(dialog)
    }

    pub(super) fn open_teardown(
        &mut self,
        cluster: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| TeardownDialog::new(cluster, workspace, cx));
        window.open_dialog(cx, move |dialog, window, cx| {
            TeardownDialog::build(&view, dialog, window, cx)
        });
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| SettingsView::new(window, cx));
        window.open_sheet(cx, move |sheet, _, _| {
            sheet.title("Settings").size(px(560.)).child(view.clone())
        });
    }

    fn render_invalid_config(
        &self,
        diagnostics: &[grove_core::DiagnosticView],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("invalid-config")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child("Grove's config has problems"),
                    )
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("Fix these files, then reload."),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .children(diagnostics.iter().enumerate().map(|(ix, d)| {
                        v_flex()
                            .id(("diagnostic", ix))
                            .gap_0p5()
                            .p_3()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .when_some(d.file.as_ref(), |this, file| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(ui::tilde(file)),
                                )
                            })
                            .child(div().text_color(theme.danger).child(d.message.clone()))
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("reload-config")
                            .label("Reload config")
                            .on_click(cx.listener(|_, _, _, cx| {
                                let core = Services::get(cx).core;
                                core.reload_config();
                            })),
                    )
                    .child(
                        Button::new("open-settings")
                            .ghost()
                            .label("Settings…")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                            ),
                    ),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot(cx);
        // GPUI Kit 0.6 leaves drawing the overlay layers to the root view.
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let theme = cx.theme();
        let with_sidebar = self.onboarding.is_none();
        if let Some(glass) = &self.glass {
            glass.set_visible(with_sidebar);
        }

        let (toolbar, main) = if let Some(onboarding) = &self.onboarding {
            (
                None,
                div()
                    .size_full()
                    .child(onboarding.clone())
                    .into_any_element(),
            )
        } else if let ConfigStatus::Invalid { diagnostics } = &snapshot.config {
            (
                None,
                self.render_invalid_config(diagnostics, cx)
                    .into_any_element(),
            )
        } else {
            let (toolbar, page) = self.render_cluster_page(&snapshot, cx);
            let main = if self.output.read(cx).is_empty() {
                div().size_full().child(page).into_any_element()
            } else {
                v_resizable("main-split")
                    .with_state(&self.split)
                    .child(resizable_panel().child(page))
                    .child(
                        resizable_panel()
                            .size(px(300.))
                            .size_range(px(120.)..px(4000.))
                            .child(self.output.clone()),
                    )
                    .into_any_element()
            };
            (toolbar, main)
        };

        // The content's top strip doubles as the title bar.
        let toolbar = self
            .toolbar_drag
            .apply(h_flex().id("toolbar"))
            .flex_none()
            .h(ui::TOOLBAR_HEIGHT)
            .px_4()
            .gap_2()
            .when(!with_sidebar, |this| this.pl(ui::TRAFFIC_LIGHTS_WIDTH))
            .when(toolbar.is_some(), |this| {
                this.border_b_1().border_color(theme.border)
            })
            .children(toolbar);

        div()
            .relative()
            .size_full()
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .size_full()
                    .items_stretch()
                    .when(with_sidebar, |this| {
                        this.child(self.render_sidebar(&snapshot, self.sidebar_state(window), cx))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .bg(theme.background)
                            .child(toolbar)
                            .child(div().flex_1().min_h_0().child(main)),
                    ),
            )
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
