//! Tearing down a cluster: show what will be removed, require confirmation
//! for uncommitted changes, then tear down.

use super::services::{Services, run_bg, run_op};
use super::ui;
use super::workspace::Workspace;
use gpui_kit::assets::IconName;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, WeakEntity, Window, div,
    px,
};
use grove_core::{TeardownOptions, TeardownReport, WorktreeReport};
use grove_git::ChangeKind;

/// Changed files listed per worktree before "and N more".
const MAX_FILES: usize = 8;

pub struct TeardownDialog {
    cluster: String,
    workspace: WeakEntity<Workspace>,
    report: Option<TeardownReport>,
    error: Option<SharedString>,
    remove_adopted: bool,
    force: bool,
}

impl TeardownDialog {
    pub fn new(cluster: String, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let core = Services::get(cx).core;
        let name = cluster.clone();
        let job = run_bg(cx, async move { core.teardown_report(&name).await });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(report)) => this.report = Some(report),
                    Ok(Err(e)) => this.error = Some(e.to_string().into()),
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
                cx.refresh_windows();
            });
        })
        .detach();
        Self {
            cluster,
            workspace,
            report: None,
            error: None,
            remove_adopted: false,
            force: false,
        }
    }

    pub fn build(view: &Entity<Self>, dialog: Dialog, _: &mut Window, cx: &mut App) -> Dialog {
        let this = view.read(cx);
        let footer = this.render_footer(view);
        dialog
            .title(format!("Tear down “{}”?", this.cluster))
            .w(px(560.))
            .child(view.clone())
            .footer(footer)
    }

    fn removed<'a>(&self, report: &'a TeardownReport) -> impl Iterator<Item = &'a WorktreeReport> {
        let remove_adopted = self.remove_adopted;
        report
            .worktrees
            .iter()
            .filter(move |w| w.will_remove || (w.adopted && remove_adopted))
    }

    fn is_dirty(&self) -> bool {
        self.report
            .as_ref()
            .is_some_and(|r| self.removed(r).any(|w| !w.changes.is_empty()))
    }

    fn can_confirm(&self) -> bool {
        self.report.is_some() && (!self.is_dirty() || self.force)
    }

    fn render_footer(&self, view: &Entity<Self>) -> AnyElement {
        let view = view.clone();
        h_flex()
            .gap_2()
            .justify_end()
            .child(
                Button::new("teardown-cancel")
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("teardown-confirm")
                    .danger()
                    .label("Tear down")
                    .disabled(!self.can_confirm())
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| this.confirm(window, cx));
                    }),
            )
            .into_any_element()
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_confirm() {
            return;
        }
        let name = self.cluster.clone();
        let opts = TeardownOptions {
            force: self.force,
            remove_adopted: self.remove_adopted,
        };
        window.close_dialog(cx);
        let _ = self.workspace.update(cx, |ws, cx| {
            if ws.selected == name {
                ws.select_cluster(grove_config::DEFAULT_CLUSTER.into(), cx);
            }
        });
        run_op(
            cx,
            format!("Couldn't tear down {name}"),
            move |core| async move { core.teardown(&name, opts).await },
        )
        .detach();
    }

    fn render_worktree(&self, w: &WorktreeReport, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let extra = w.changes.len().saturating_sub(MAX_FILES);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().font_medium().child(w.project.clone()))
                    .when_some(w.branch.clone(), |this, b| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(ui::inline_icon(IconName::GitBranch))
                                .child(b),
                        )
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(ui::tilde(&w.path)),
            )
            .when(!w.changes.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_0p5()
                        .text_sm()
                        .child(div().text_color(theme.warning).child(format!(
                            "{} uncommitted {}",
                            w.changes.len(),
                            if w.changes.len() == 1 {
                                "change"
                            } else {
                                "changes"
                            }
                        )))
                        .children(w.changes.iter().take(MAX_FILES).map(|f| {
                            h_flex()
                                .gap_2()
                                .font_family(theme.mono_font_family.clone())
                                .child(
                                    div()
                                        .w_4()
                                        .text_color(theme.muted_foreground)
                                        .child(change_letter(f.kind)),
                                )
                                .child(div().truncate().child(f.path.clone()))
                        }))
                        .when(extra > 0, |this| {
                            this.child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("and {extra} more")),
                            )
                        }),
                )
            })
    }
}

impl Render for TeardownDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(report) = &self.report else {
            return v_flex()
                .gap_2()
                .child(match &self.error {
                    Some(error) => div()
                        .text_color(theme.danger)
                        .child(error.clone())
                        .into_any_element(),
                    None => h_flex()
                        .gap_2()
                        .text_color(theme.muted_foreground)
                        .child(Spinner::new().with_size(Size::Size(ui::ICON)))
                        .child("Checking for uncommitted changes…")
                        .into_any_element(),
                })
                .into_any_element();
        };
        let removed: Vec<&WorktreeReport> = self.removed(report).collect();
        let kept: Vec<&WorktreeReport> = report
            .worktrees
            .iter()
            .filter(|w| !(w.will_remove || (w.adopted && self.remove_adopted)))
            .collect();
        let has_adopted = report.worktrees.iter().any(|w| w.adopted);
        let dirty = self.is_dirty();

        v_flex()
            .gap_4()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Stops its processes and runs teardown hooks, then removes:"),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title("Worktrees", cx))
                    .when(removed.is_empty(), |this| {
                        this.child(div().text_color(theme.muted_foreground).child("None"))
                    })
                    .children(removed.iter().map(|w| self.render_worktree(w, cx))),
            )
            .when(!report.databases.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(ui::section_title("Databases to drop", cx))
                        .children(report.databases.iter().map(|db| {
                            h_flex()
                                .gap_1()
                                .child(ui::inline_icon(IconName::Database))
                                .child(db.clone())
                        })),
                )
            })
            .when(!kept.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(ui::section_title("Kept (adopted from other tools)", cx))
                        .children(kept.iter().map(|w| {
                            div()
                                .text_sm()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(ui::tilde(&w.path))
                        })),
                )
            })
            .when(has_adopted, |this| {
                this.child(
                    Checkbox::new("remove-adopted")
                        .label("Also remove adopted worktrees")
                        .checked(self.remove_adopted)
                        .on_change(cx.listener(|this, checked: &bool, window, cx| {
                            this.remove_adopted = *checked;
                            cx.notify();
                            window.refresh();
                        })),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Branches are kept."),
            )
            .when(dirty, |this| {
                this.child(
                    Checkbox::new("force-teardown")
                        .label("Discard uncommitted changes")
                        .checked(self.force)
                        .on_change(cx.listener(|this, checked: &bool, window, cx| {
                            this.force = *checked;
                            cx.notify();
                            window.refresh();
                        })),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_color(theme.danger).child(error))
            })
            .into_any_element()
    }
}

fn change_letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Modified => "M",
        ChangeKind::Added => "A",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Untracked => "?",
        ChangeKind::Conflicted => "U",
        ChangeKind::Other => "·",
    }
}
