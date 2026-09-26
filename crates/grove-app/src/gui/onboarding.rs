//! First run: workspace → projects → certificate → start.

use super::app::start_cluster;
use super::services::{Services, run_bg};
use super::settings::{ProjectRow, check_trusted, install_trust, load_projects, project_row};
use super::ui;
use gpui_kit::assets::IconName;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Subscription,
    WeakEntity, Window, div, rems,
};
use grove_core::{ConfigStatus, UserConfigDraft};
use std::collections::HashSet;

pub enum OnboardingEvent {
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Workspace,
    Projects,
    Certificate,
    Finish,
}

const STEPS: [(Step, &str); 4] = [
    (Step::Workspace, "Workspace"),
    (Step::Projects, "Projects"),
    (Step::Certificate, "Certificate"),
    (Step::Finish, "Start"),
];

pub struct Onboarding {
    step: Step,
    workspace: Entity<InputState>,
    clones_root: Entity<InputState>,
    busy: bool,
    error: Option<SharedString>,
    projects: Result<Vec<ProjectRow>, String>,
    enabled: HashSet<String>,
    cloning: HashSet<String>,
    trusted: Option<bool>,
    trusting: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OnboardingEvent> for Onboarding {}

impl Onboarding {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let core = Services::get(cx).core;
        let draft = core.user_config_draft();
        // A workspace that's configured but missing (e.g. moved) is shown
        // as the error to fix.
        let error = match core.snapshot().config {
            ConfigStatus::WorkspaceMissing { message } => Some(message.into()),
            _ => None,
        };
        let workspace = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("git@github.com:acme/grove-workspace.git or ~/path/to/workspace")
                .default_value(draft.workspace.clone())
        });
        let clones_root = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("~/Developer")
                .default_value(draft.clones_root.clone())
        });
        let on_enter = |this: &mut Self,
                        _: &Entity<InputState>,
                        event: &InputEvent,
                        _: &mut Window,
                        cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = event
                && this.step == Step::Workspace
            {
                this.save_workspace(cx);
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&workspace, window, on_enter),
            cx.subscribe_in(&clones_root, window, on_enter),
        ];
        workspace.update(cx, |input, cx| input.focus(window, cx));
        Self {
            step: Step::Workspace,
            workspace,
            clones_root,
            busy: false,
            error,
            projects: Ok(Vec::new()),
            enabled: HashSet::new(),
            cloning: HashSet::new(),
            trusted: None,
            trusting: false,
            _subscriptions: subscriptions,
        }
    }

    /// Writes the workspace and clones root, then fetches the workspace.
    fn save_workspace(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let workspace = self.workspace.read(cx).value().trim().to_string();
        let clones_root = self.clones_root.read(cx).value().trim().to_string();
        if workspace.is_empty() {
            self.error = Some("Enter your team's workspace: a git URL or a local path.".into());
            cx.notify();
            return;
        }
        let core = Services::get(cx).core;
        let draft = UserConfigDraft {
            workspace,
            clones_root,
            projects: None,
            ..core.user_config_draft()
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let job = run_bg(cx, async move {
            core.write_user_config(&draft).map_err(|e| e.to_string())?;
            core.sync_workspace().await.map_err(|e| e.to_string())
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = job.await.and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        this.reload_projects(cx);
                        this.step = Step::Projects;
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_projects(&mut self, cx: &mut Context<Self>) {
        let core = Services::get(cx).core;
        self.projects = load_projects(&core);
        if let Ok(list) = &self.projects
            && self.enabled.is_empty()
        {
            self.enabled = list
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.name.clone())
                .collect();
        }
        cx.notify();
    }

    fn clone_project(&mut self, name: String, cx: &mut Context<Self>) {
        if !self.cloning.insert(name.clone()) {
            return;
        }
        cx.notify();
        let core = Services::get(cx).core;
        let n = name.clone();
        let job = run_bg(cx, async move { core.clone_project(&n).await });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.cloning.remove(&name);
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => this.error = Some(format!("Couldn't clone {name}: {e}").into()),
                    Err(e) => this.error = Some(e.into()),
                }
                this.reload_projects(cx);
            });
        })
        .detach();
    }

    /// Saves the project selection and moves on to the certificate.
    fn save_projects(&mut self, cx: &mut Context<Self>) {
        let Ok(list) = &self.projects else {
            self.step = Step::Certificate;
            return;
        };
        if !list.iter().all(|p| self.enabled.contains(&p.name)) {
            let core = Services::get(cx).core;
            let draft = UserConfigDraft {
                projects: Some(
                    list.iter()
                        .filter(|p| self.enabled.contains(&p.name))
                        .map(|p| p.name.clone())
                        .collect(),
                ),
                ..core.user_config_draft()
            };
            if let Err(e) = core.write_user_config(&draft) {
                self.error = Some(e.to_string().into());
                cx.notify();
                return;
            }
        }
        self.error = None;
        self.step = Step::Certificate;
        let task = check_trusted(cx);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let trusted = task.await;
            let _ = this.update(cx, |this, cx| {
                this.trusted = trusted;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn trust(&mut self, cx: &mut Context<Self>) {
        self.trusting = true;
        self.error = None;
        cx.notify();
        let task = install_trust(cx);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let recheck = this.update(cx, |this, cx| {
                this.trusting = false;
                if let Err(e) = result {
                    this.error = Some(format!("Couldn't trust the certificate: {e}").into());
                }
                cx.notify();
                check_trusted(cx)
            });
            if let Ok(recheck) = recheck {
                let trusted = recheck.await;
                let _ = this.update(cx, |this, cx| {
                    this.trusted = trusted;
                    if trusted == Some(true) {
                        this.step = Step::Finish;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn finish(&mut self, start: bool, cx: &mut Context<Self>) {
        if start {
            start_cluster(cx, grove_config::DEFAULT_CLUSTER.to_string());
        }
        cx.emit(OnboardingEvent::Done);
    }

    fn render_steps(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let current = STEPS.iter().position(|(s, _)| *s == self.step).unwrap_or(0);
        h_flex()
            .gap_2()
            .text_sm()
            .children(STEPS.iter().enumerate().map(|(ix, (_, label))| {
                let color = if ix == current {
                    theme.foreground
                } else {
                    theme.muted_foreground
                };
                h_flex()
                    .gap_2()
                    .text_color(color)
                    .when(ix > 0, |this| this.child("›"))
                    .child(
                        div()
                            .when(ix == current, |this| this.font_semibold())
                            .child(format!("{}. {label}", ix + 1)),
                    )
            }))
    }

    fn render_workspace(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(heading(
                "Welcome to Grove",
                "Grove runs copies of your team's dev environment side by side. Start by pointing it at your workspace: the repo (or folder) with grove.toml and projects/*.toml.",
                cx,
            ))
            .child(
                Form::vertical()
                    .child(
                        Field::new()
                            .label("Workspace")
                            .description("A git URL is cloned to ~/.grove/workspace.")
                            .child(Input::new(&self.workspace)),
                    )
                    .child(
                        Field::new()
                            .label("Clones root")
                            .description("Where your main clones live, e.g. ~/Developer/<project>.")
                            .child(Input::new(&self.clones_root)),
                    ),
            )
            .child(
                h_flex().child(
                    Button::new("onboarding-workspace")
                        .primary()
                        .label("Continue")
                        .loading(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.save_workspace(cx))),
                ),
            )
            .into_any_element()
    }

    fn render_projects(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let list: AnyElement = match &self.projects {
            Err(e) => div()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element(),
            Ok(list) => v_flex()
                .gap_1()
                .children(list.iter().map(|p| {
                    project_row(
                        p,
                        self.enabled.contains(&p.name),
                        self.cloning.contains(&p.name),
                        cx.listener({
                            let name = p.name.clone();
                            move |this: &mut Self, checked: &bool, _, cx| {
                                if *checked {
                                    this.enabled.insert(name.clone());
                                } else {
                                    this.enabled.remove(&name);
                                }
                                cx.notify();
                            }
                        }),
                        cx.listener({
                            let name = p.name.clone();
                            move |this: &mut Self, _, _, cx| this.clone_project(name.clone(), cx)
                        }),
                        cx,
                    )
                }))
                .into_any_element(),
        };
        let missing = self
            .projects
            .as_ref()
            .map(|l| {
                l.iter()
                    .filter(|p| self.enabled.contains(&p.name) && !p.cloned)
                    .count()
            })
            .unwrap_or(0);
        v_flex()
            .gap_4()
            .child(heading(
                "Pick your projects",
                "Grove runs the projects you enable here. Clone the ones you don't have yet.",
                cx,
            ))
            .child(list)
            .when(missing > 0, |this| {
                this.child(div().text_sm().text_color(theme.muted_foreground).child(
                    "Projects that aren't cloned can't run; you can clone them later in Settings.",
                ))
            })
            .child(
                h_flex().gap_2().child(
                    Button::new("onboarding-projects")
                        .primary()
                        .label("Continue")
                        .on_click(cx.listener(|this, _, _, cx| this.save_projects(cx))),
                ),
            )
            .into_any_element()
    }

    fn render_certificate(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .gap_4()
            .child(heading(
                "Trust Grove's certificate",
                "Grove serves every cluster over HTTPS on *.localhost with its own certificate authority. Adding it to your login keychain stops browser warnings; macOS asks for your password once.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(match self.trusted {
                        Some(true) => Icon::new(IconName::ShieldCheck)
                            .size(ui::ICON)
                            .text_color(theme.success)
                            .into_any_element(),
                        Some(false) => Icon::new(IconName::Shield)
                            .size(ui::ICON)
                            .text_color(theme.warning)
                            .into_any_element(),
                        None => Spinner::new()
                            .with_size(Size::Size(ui::ICON))
                            .into_any_element(),
                    })
                    .child(match self.trusted {
                        Some(true) => "Trusted",
                        Some(false) => "Not trusted yet",
                        None => "Checking…",
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(self.trusted != Some(true), |this| {
                        this.child(
                            Button::new("onboarding-trust")
                                .primary()
                                .label("Trust certificate…")
                                .loading(self.trusting)
                                .on_click(cx.listener(|this, _, _, cx| this.trust(cx))),
                        )
                    })
                    .child(
                        Button::new("onboarding-cert-next")
                            .when(self.trusted == Some(true), |b| b.primary())
                            .when(self.trusted != Some(true), |b| b.ghost())
                            .label(if self.trusted == Some(true) {
                                "Continue"
                            } else {
                                "Skip for now"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = Step::Finish;
                                this.error = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_finish(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(heading(
                "Ready",
                "The default cluster runs every enabled project from its main clone. Start it now, or create a cluster for a pull request or branch from the sidebar.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("onboarding-start")
                            .primary()
                            .label("Start default cluster")
                            .on_click(cx.listener(|this, _, _, cx| this.finish(true, cx))),
                    )
                    .child(
                        Button::new("onboarding-done")
                            .ghost()
                            .label("Not now")
                            .on_click(cx.listener(|this, _, _, cx| this.finish(false, cx))),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Onboarding {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.step {
            Step::Workspace => self.render_workspace(cx),
            Step::Projects => self.render_projects(cx),
            Step::Certificate => self.render_certificate(cx),
            Step::Finish => self.render_finish(cx),
        };
        let theme = cx.theme();
        div()
            .id("onboarding")
            .size_full()
            .child(
                v_flex().w_full().items_center().py_12().px_6().child(
                    v_flex()
                        .w_full()
                        .max_w(rems(34.))
                        .gap_6()
                        .child(self.render_steps(cx))
                        .child(body)
                        .when_some(self.error.clone(), |this, error| {
                            this.child(
                                h_flex()
                                    .gap_2()
                                    .items_start()
                                    .text_color(theme.danger)
                                    .child(Icon::new(IconName::CircleAlert).size(ui::ICON))
                                    .child(div().flex_1().child(error)),
                            )
                        }),
                ),
            )
            .overflow_y_scrollbar()
    }
}

fn heading(title: &'static str, body: &'static str, cx: &Context<Onboarding>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_lg().font_semibold().child(title))
        .child(div().text_color(cx.theme().muted_foreground).child(body))
}
