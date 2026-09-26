//! The source-list sidebar: workspace, clusters, proxy status, settings.
//! Full height and edge to edge like macOS 27's: Liquid Glass when AppKit
//! has it, a tinted blur otherwise.

use super::ui::{self, SIDEBAR_ICON, SIDEBAR_ROW, TOOLBAR_HEIGHT};
use super::workspace::Workspace;
use gpui_kit::assets::IconName;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, ElementExt as _, Icon, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Hsla, IntoElement, SharedString, Window, div, px, rems};
use grove_core::{ClusterOrigin, ClusterPhase, ClusterView, ConfigStatus, ProxyStatus, Snapshot};

gpui_kit::actions!(
    grove_sidebar,
    [
        /// Select the cluster above the selected one.
        SelectPreviousCluster,
        /// Select the cluster below the selected one.
        SelectNextCluster,
    ]
);

/// Key context of the focused cluster list.
pub const CLUSTER_LIST_CONTEXT: &str = "ClusterList";

/// How much of the sidebar colour covers the window blur (fallback).
const BLUR_TINT: f32 = 0.85;
/// How strongly the Liquid Glass is tinted with the sidebar colour, in light
/// and dark. GPUI's text isn't vibrant (it doesn't flip over dark
/// wallpaper), so the glass needs a predictable tone for contrast.
const GLASS_TINT: (f32, f32) = (0.6, 0.6);

/// How the sidebar's rows and icons are drawn this frame.
#[derive(Clone, Copy)]
pub(super) struct SidebarState {
    /// The window is key: icons show their colour.
    pub active: bool,
    /// The cluster list has keyboard focus: the selection is accent-filled.
    pub list_focused: bool,
}

/// `default` first, then newest first.
pub(super) fn sorted_clusters(snapshot: &Snapshot) -> Vec<&ClusterView> {
    let mut clusters: Vec<&ClusterView> = snapshot.clusters.iter().collect();
    clusters.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then(b.created_at.cmp(&a.created_at))
    });
    clusters
}

impl Workspace {
    pub(super) fn render_sidebar(
        &self,
        snapshot: &Snapshot,
        state: SidebarState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let rows = sorted_clusters(snapshot)
            .into_iter()
            .map(|c| self.render_cluster_row(c, state, cx))
            .collect::<Vec<_>>();
        let workspace: SharedString = match &snapshot.config {
            ConfigStatus::Loaded { workspace, .. } => workspace.clone().into(),
            _ => "Grove".into(),
        };

        let sidebar = v_flex()
            .flex_none()
            .w(rems(17.))
            .h_full()
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            // The traffic lights sit in this strip.
            .child(
                self.sidebar_drag
                    .apply(div().id("sidebar-top"))
                    .flex_none()
                    .h(TOOLBAR_HEIGHT),
            )
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .pt_1()
                    .pb_1()
                    .child(ui::section_title(workspace, cx)),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("cluster-list")
                        .track_focus(&self.cluster_list_focus)
                        .key_context(CLUSTER_LIST_CONTEXT)
                        .on_action(cx.listener(|this, _: &SelectPreviousCluster, _, cx| {
                            this.select_adjacent_cluster(-1, cx)
                        }))
                        .on_action(cx.listener(|this, _: &SelectNextCluster, _, cx| {
                            this.select_adjacent_cluster(1, cx)
                        }))
                        .size_full()
                        .px_2()
                        .gap_px()
                        .overflow_y_scrollbar()
                        .children(rows),
                ),
            )
            .child(self.render_sidebar_footer(snapshot, state, cx));

        match self.glass.clone() {
            // The glass sits under GPUI's view: leave the column clear and
            // keep the glass on the column's bounds.
            Some(glass) => {
                let tint = if theme.is_dark() {
                    GLASS_TINT.1
                } else {
                    GLASS_TINT.0
                };
                glass.set_tint(theme.sidebar.opacity(tint));
                sidebar
                    .on_prepaint(move |bounds, window, _| {
                        glass.set_frame(bounds, window.viewport_size().height);
                    })
                    .into_any_element()
            }
            None => sidebar
                .bg(theme.sidebar.opacity(BLUR_TINT))
                .into_any_element(),
        }
    }

    /// Proxy status and the Settings item.
    fn render_sidebar_footer(
        &self,
        snapshot: &Snapshot,
        state: SidebarState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (color, text): (_, SharedString) = match &snapshot.proxy {
            ProxyStatus::Running { https } => (
                theme.success,
                match https.first().and_then(|a| a.rsplit(':').next()) {
                    Some("443") | None => "HTTPS proxy".into(),
                    Some(port) => format!("HTTPS proxy on :{port}").into(),
                },
            ),
            ProxyStatus::Failed { .. } => (theme.danger, "Proxy stopped".into()),
            ProxyStatus::Disabled => (theme.muted_foreground, "Proxy off".into()),
        };
        let tooltip: SharedString = match &snapshot.proxy {
            ProxyStatus::Running { https } => format!("Listening on {}", https.join(", ")).into(),
            ProxyStatus::Failed { message } => message.clone().into(),
            ProxyStatus::Disabled => "The HTTPS proxy isn't running".into(),
        };
        v_flex()
            .flex_none()
            .px_2()
            .pb_2()
            .gap_px()
            .child(
                h_flex()
                    .id("proxy-status")
                    .h(SIDEBAR_ROW)
                    .px_2()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .flex_none()
                            .w(SIDEBAR_ICON)
                            .flex()
                            .justify_center()
                            .child(ui::dot(color)),
                    )
                    .child(div().truncate().child(text))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
            )
            .child(
                h_flex()
                    .id("sidebar-settings")
                    .h(SIDEBAR_ROW)
                    .px_2()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .child(sidebar_icon(
                        IconName::Settings,
                        icon_color(state, false, cx),
                    ))
                    .child("Settings")
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
    }

    fn render_cluster_row(
        &self,
        c: &ClusterView,
        state: SidebarState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let selected = self.selected == c.name;
        // macOS source lists: an accent pill while the list has focus, a
        // grey one otherwise.
        let emphasized = selected && state.list_focused && state.active;
        let name = c.name.clone();
        let busy = matches!(
            c.phase,
            ClusterPhase::Starting
                | ClusterPhase::Stopping
                | ClusterPhase::Creating { .. }
                | ClusterPhase::TearingDown
        );
        let id: SharedString = format!("cluster-row-{}", c.name).into();
        let icon = match &c.origin {
            ClusterOrigin::Default => IconName::House,
            ClusterOrigin::Branch { .. } => IconName::GitBranch,
            ClusterOrigin::PullRequest { .. } => IconName::GitPullRequest,
            ClusterOrigin::Manual => IconName::Layers,
        };
        let tooltip: SharedString =
            format!("{} · {}", ui::origin_label(&c.origin), c.phase.label()).into();

        h_flex()
            .id(id)
            .flex_none()
            .h(SIDEBAR_ROW)
            .px_2()
            .gap_2()
            .rounded(theme.radius_lg)
            .when(selected && !emphasized, |this| {
                this.bg(theme.sidebar_accent)
            })
            .when(emphasized, |this| {
                this.bg(theme.primary).text_color(theme.primary_foreground)
            })
            .child(sidebar_icon(icon, icon_color(state, emphasized, cx)))
            .child(div().flex_1().min_w_0().truncate().child(c.name.clone()))
            // The cluster's state, as a small trailing indicator.
            .child(
                div()
                    .flex_none()
                    .w(SIDEBAR_ICON)
                    .flex()
                    .justify_center()
                    .child(if busy {
                        Spinner::new()
                            .with_size(Size::Size(px(12.)))
                            .into_any_element()
                    } else {
                        ui::dot(ui::phase_color(&c.phase, cx)).into_any_element()
                    }),
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.cluster_list_focus.focus(window, cx);
                this.select_cluster(name.clone(), cx);
            }))
    }

    /// Arrow keys in the focused cluster list.
    fn select_adjacent_cluster(&mut self, step: isize, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(cx);
        let names: Vec<String> = sorted_clusters(&snapshot)
            .into_iter()
            .map(|c| c.name.clone())
            .collect();
        let Some(current) = names.iter().position(|n| *n == self.selected) else {
            return;
        };
        let next = current as isize + step;
        if let Some(name) = usize::try_from(next).ok().and_then(|ix| names.get(ix)) {
            self.select_cluster(name.clone(), cx);
        }
    }

    /// The sidebar's look for this frame.
    pub(super) fn sidebar_state(&self, window: &Window) -> SidebarState {
        SidebarState {
            active: window.is_window_active(),
            list_focused: self.cluster_list_focus.is_focused(window),
        }
    }
}

/// macOS 27 sidebar icons: the accent colour while the window is key, grey
/// otherwise, and white on an accent selection.
fn icon_color(state: SidebarState, emphasized: bool, cx: &Context<Workspace>) -> Hsla {
    let theme = cx.theme();
    if emphasized {
        theme.primary_foreground
    } else if state.active {
        theme.primary
    } else {
        theme.muted_foreground
    }
}

fn sidebar_icon(icon: IconName, color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .w(SIDEBAR_ICON)
        .flex()
        .justify_center()
        .child(Icon::new(icon).size(SIDEBAR_ICON).text_color(color))
}
