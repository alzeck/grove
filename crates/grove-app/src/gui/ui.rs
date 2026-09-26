//! Small presentational pieces shared by the screens.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, StyledExt as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Div, ElementId, Hsla, IntoElement, MouseButton, Pixels, SharedString, Stateful, div, px,
};
use grove_core::{ActivityState, ClusterOrigin, ClusterPhase, ProcState};
use std::cell::Cell;
use std::rc::Rc;

// Window chrome and icon geometry, in points, matching macOS 27's Finder:
// they line up with native parts (traffic lights, the system's icon and
// hit-target sizes), so they are pixels rather than rems.

/// Height of the unified title bar: the sidebar's top strip and the content
/// toolbar.
pub const TOOLBAR_HEIGHT: Pixels = px(52.);
/// Toolbar capsules and the buttons in them.
pub const TOOLBAR_CAPSULE: Pixels = px(36.);
pub const TOOLBAR_BUTTON: Pixels = px(32.);
pub const TOOLBAR_ICON: Pixels = px(18.);
/// Icon-only buttons elsewhere: the smallest comfortable hit target.
pub const BUTTON: Pixels = px(28.);
pub const ICON: Pixels = px(16.);
/// Glyphs inline with 11–12pt metadata text.
pub const INLINE_ICON: Pixels = px(14.);
/// Sidebar rows and their icons.
pub const SIDEBAR_ROW: Pixels = px(30.);
pub const SIDEBAR_ICON: Pixels = px(18.);
/// Height of AppKit's traffic-light buttons.
pub const TRAFFIC_LIGHT_SIZE: Pixels = px(14.);
/// Distance from the window's leading edge to the close button.
pub const TRAFFIC_LIGHTS_INSET: Pixels = px(18.);
/// Room the traffic lights take when no sidebar sits under them.
pub const TRAFFIC_LIGHTS_WIDTH: Pixels = px(78.);

/// Makes an element behave like a title bar: dragging it moves the window
/// and double-clicking zooms it (or minimizes, per System Settings). The
/// window is opened with `app_owns_titlebar_drag`, so these areas are the
/// only places the window can be dragged from.
///
/// Controls inside should be wrapped in [`no_drag`] so pressing them doesn't
/// start a move.
#[derive(Clone, Default)]
pub struct WindowDrag {
    /// A press started in this area and hasn't moved or been released yet.
    armed: Rc<Cell<bool>>,
}

impl WindowDrag {
    pub fn apply(&self, element: Stateful<Div>) -> Stateful<Div> {
        let (down, up, out, moved) = (
            self.armed.clone(),
            self.armed.clone(),
            self.armed.clone(),
            self.armed.clone(),
        );
        element
            .on_mouse_down(MouseButton::Left, move |event, window, _| {
                if event.click_count == 2 {
                    down.set(false);
                    window.titlebar_double_click();
                } else {
                    down.set(true);
                }
            })
            .on_mouse_up(MouseButton::Left, move |_, _, _| up.set(false))
            .on_mouse_down_out(move |_, _, _| out.set(false))
            .on_mouse_move(move |event, window, _| {
                if moved.get() && event.pressed_button == Some(MouseButton::Left) {
                    moved.set(false);
                    window.start_window_move();
                }
            })
    }
}

/// An icon-only button (label in the tooltip): a 16pt glyph in a 28pt hit
/// target, or a spinner while `loading`.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: impl Into<SharedString>,
    loading: bool,
) -> Button {
    sized_icon_button(id, icon, tooltip, loading, BUTTON, ICON)
}

/// A toolbar button: an 18pt glyph in a 32pt pill, for [`capsule`]s.
pub fn toolbar_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: impl Into<SharedString>,
    loading: bool,
) -> Button {
    sized_icon_button(id, icon, tooltip, loading, TOOLBAR_BUTTON, TOOLBAR_ICON)
        .rounded(ButtonRounded::Size(TOOLBAR_BUTTON / 2.))
}

fn sized_icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: impl Into<SharedString>,
    loading: bool,
    hit: Pixels,
    glyph: Pixels,
) -> Button {
    // The icon goes in as a child: `Button::icon` scales it to 3/4 of the
    // button, bigger than macOS draws glyphs in controls this size.
    let content = if loading {
        Spinner::new()
            .with_size(Size::Size(glyph))
            .into_any_element()
    } else {
        Icon::new(icon).size(glyph).into_any_element()
    };
    Button::new(id)
        .ghost()
        .with_size(Size::Size(hit))
        .w(hit)
        .h(hit)
        .tooltip(tooltip)
        .loading(loading)
        .child(content)
}

/// A group of toolbar buttons in one glass-like capsule, as in Finder's
/// toolbar. A single button makes a round button.
pub fn capsule(cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h(TOOLBAR_CAPSULE)
        .p(px(2.))
        .gap_px()
        .rounded_full()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
}

/// A glyph sized for inline metadata.
pub fn inline_icon(icon: IconName) -> Icon {
    Icon::new(icon).size(INLINE_ICON)
}

/// Keeps presses on the controls in `element` from dragging the window.
pub fn no_drag(element: Div) -> Div {
    element.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Colour for a cluster phase. Always shown next to the phase's name, so
/// colour is never the only cue.
pub fn phase_color(phase: &ClusterPhase, cx: &App) -> Hsla {
    let theme = cx.theme();
    match phase {
        ClusterPhase::Running => theme.success,
        ClusterPhase::Starting
        | ClusterPhase::Stopping
        | ClusterPhase::Creating { .. }
        | ClusterPhase::TearingDown => theme.warning,
        ClusterPhase::Degraded | ClusterPhase::Error { .. } => theme.danger,
        ClusterPhase::Idle => theme.info,
        ClusterPhase::Stopped => theme.muted_foreground.opacity(0.6),
    }
}

pub fn proc_color(state: &ProcState, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        ProcState::Ready => theme.success,
        ProcState::Starting | ProcState::Waiting => theme.warning,
        ProcState::Exited { success: true, .. } | ProcState::Stopped => {
            theme.muted_foreground.opacity(0.6)
        }
        ProcState::Exited { .. } | ProcState::Failed { .. } => theme.danger,
    }
}

/// A process state with its detail, e.g. `crashed (exit code 1)`.
pub fn proc_state_text(state: &ProcState) -> String {
    match state {
        ProcState::Exited { status, .. } => format!("{} ({status})", state.label()),
        ProcState::Failed { message } => format!("failed: {message}"),
        other => other.label().to_string(),
    }
}

/// A small filled circle.
pub fn dot(color: Hsla) -> Div {
    div().flex_none().size_2().rounded_full().bg(color)
}

/// "Main clones", "PR #123 · api", "feat/login"…
pub fn origin_label(origin: &ClusterOrigin) -> String {
    match origin {
        ClusterOrigin::Default => "Main clones".into(),
        ClusterOrigin::Branch { branch } => branch.clone(),
        ClusterOrigin::PullRequest {
            project, number, ..
        } => format!("PR #{number} · {project}"),
        ClusterOrigin::Manual => "Custom".into(),
    }
}

pub fn activity_color(state: &ActivityState, cx: &App) -> Hsla {
    match state {
        ActivityState::Running => cx.theme().warning,
        ActivityState::Succeeded => cx.theme().success,
        ActivityState::Failed { .. } => cx.theme().danger,
    }
}

/// "just now", "5m ago", "2h ago", "3d ago".
pub fn ago(timestamp: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(timestamp);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// A section heading, like the group headings of a macOS sidebar.
pub fn section_title(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .text_sm()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// Shortens a path under the home directory to `~/…`.
pub fn tilde(path: &std::path::Path) -> String {
    let home = grove_config::expand_tilde("~");
    match path.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}
