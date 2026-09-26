//! The new-cluster dialog: pick a PR or branch, review the plan, create.

use super::services::{Services, run_bg, run_op};
use super::ui;
use super::workspace::Workspace;
use gpui_kit::assets::IconName;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, AsyncWindowContext, Context, Entity, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, div, px,
};
use grove_core::{
    ClusterPlan, DbMode, ExternalWorktree, NewClusterSource, PlannedCheckout, WorktreeSource,
};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    PullRequest,
    Branch,
}

enum Stage {
    Source,
    Planning,
    Plan(Box<PlanEdit>),
}

struct PlanEdit {
    plan: ClusterPlan,
    /// What the planner proposed for each project, offered as a choice.
    proposed: HashMap<String, PlannedCheckout>,
}

pub struct NewClusterDialog {
    workspace: WeakEntity<Workspace>,
    kind: SourceKind,
    pr_input: Entity<InputState>,
    branch_input: Entity<InputState>,
    base_input: Entity<InputState>,
    project: Entity<SelectState<Vec<String>>>,
    name_input: Entity<InputState>,
    stage: Stage,
    error: Option<SharedString>,
    /// Default database mode of projects that have a database.
    db_defaults: HashMap<String, DbMode>,
    external: Vec<ExternalWorktree>,
    _subscriptions: Vec<Subscription>,
}

impl NewClusterDialog {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let core = Services::get(cx).core;
        let (projects, db_defaults) = match core.config() {
            Some(cfg) => (
                cfg.projects.keys().cloned().collect::<Vec<_>>(),
                cfg.projects
                    .iter()
                    .filter_map(|(name, p)| {
                        p.config
                            .database
                            .as_ref()
                            .map(|db| (name.clone(), db.default))
                    })
                    .collect(),
            ),
            None => (Vec::new(), HashMap::new()),
        };
        let pr_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("https://github.com/org/repo/pull/123 or #123")
        });
        let branch_input = cx.new(|cx| InputState::new(window, cx).placeholder("feature/login"));
        let base_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("The project's default branch"));
        let name_input = cx.new(|cx| InputState::new(window, cx));
        let project = cx.new(|cx| SelectState::new(projects, None, window, cx));

        let on_enter = |this: &mut Self,
                        _: &Entity<InputState>,
                        event: &InputEvent,
                        window: &mut Window,
                        cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = event {
                this.primary(window, cx);
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&pr_input, window, on_enter),
            cx.subscribe_in(&branch_input, window, on_enter),
            cx.subscribe_in(&base_input, window, on_enter),
            cx.subscribe_in(&name_input, window, on_enter),
        ];

        // Worktrees made by other tools, offered as checkouts.
        let job = run_bg(cx, async move { core.external_worktrees().await });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            if let Ok(Ok(list)) = job.await {
                let _ = this.update(cx, |this, cx| {
                    this.external = list;
                    cx.notify();
                });
            }
        })
        .detach();

        pr_input.update(cx, |input, cx| input.focus(window, cx));

        Self {
            workspace,
            kind: SourceKind::PullRequest,
            pr_input,
            branch_input,
            base_input,
            project,
            name_input,
            stage: Stage::Source,
            error: None,
            db_defaults,
            external: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The dialog around the view. Rebuilt on every render of the window.
    pub fn build(view: &Entity<Self>, dialog: Dialog, _: &mut Window, cx: &mut App) -> Dialog {
        let footer = view.read(cx).render_footer(view);
        dialog
            .title("New cluster")
            .w(px(620.))
            .overlay_closable(false)
            .child(view.clone())
            .footer(footer)
    }

    fn render_footer(&self, view: &Entity<Self>) -> AnyElement {
        let cancel = Button::new("new-cluster-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        let primary = {
            let view = view.clone();
            let (label, loading) = match &self.stage {
                Stage::Source => ("Continue", false),
                Stage::Planning => ("Continue", true),
                Stage::Plan(_) => ("Create", false),
            };
            Button::new("new-cluster-primary")
                .primary()
                .label(label)
                .loading(loading)
                .on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| this.primary(window, cx));
                })
        };
        h_flex()
            .w_full()
            .justify_between()
            .child(match &self.stage {
                Stage::Plan(_) => {
                    let view = view.clone();
                    ui::icon_button("new-cluster-back", IconName::ChevronLeft, "Back", false)
                        .on_click(move |_, window, cx| {
                            view.update(cx, |this, cx| {
                                this.stage = Stage::Source;
                                this.error = None;
                                cx.notify();
                            });
                            window.refresh();
                        })
                        .into_any_element()
                }
                _ => div().into_any_element(),
            })
            .child(h_flex().gap_2().child(cancel).child(primary))
            .into_any_element()
    }

    /// Continue (plan) or Create, depending on the stage.
    fn primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.stage {
            Stage::Source => self.plan(window, cx),
            Stage::Planning => {}
            Stage::Plan(_) => self.create(window, cx),
        }
        window.refresh();
    }

    fn plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.read(cx).selected_value().cloned();
        let value =
            |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();
        let source = match self.kind {
            SourceKind::PullRequest => {
                let input = value(&self.pr_input, cx);
                if input.is_empty() {
                    self.error = Some("Paste a pull request URL or number.".into());
                    cx.notify();
                    return;
                }
                NewClusterSource::PullRequest { input, project }
            }
            SourceKind::Branch => {
                let branch = value(&self.branch_input, cx);
                if branch.is_empty() {
                    self.error = Some("Enter a branch name.".into());
                    cx.notify();
                    return;
                }
                let base = Some(value(&self.base_input, cx)).filter(|b| !b.is_empty());
                NewClusterSource::Branch {
                    branch,
                    project,
                    base,
                }
            }
        };
        self.stage = Stage::Planning;
        self.error = None;
        cx.notify();

        let core = Services::get(cx).core;
        let job = run_bg(cx, async move { core.plan_cluster(source).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(plan)) => {
                        this.name_input.update(cx, |input, cx| {
                            input.set_value(plan.name.clone(), window, cx)
                        });
                        let proposed = plan
                            .projects
                            .iter()
                            .map(|(name, p)| (name.clone(), p.checkout.clone()))
                            .collect();
                        this.stage = Stage::Plan(Box::new(PlanEdit { plan, proposed }));
                    }
                    Ok(Err(e)) => {
                        this.stage = Stage::Source;
                        this.error = Some(e.to_string().into());
                    }
                    Err(e) => {
                        this.stage = Stage::Source;
                        this.error = Some(e.into());
                    }
                }
                cx.notify();
                window.refresh();
            });
        })
        .detach();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Stage::Plan(edit) = &self.stage else {
            return;
        };
        let name = self.name_input.read(cx).value().trim().to_string();
        if !grove_config::is_valid_cluster_name(&name) {
            self.error =
                Some("Use lowercase letters, digits and dashes (at most 30 characters).".into());
            cx.notify();
            return;
        }
        let taken = name == grove_config::DEFAULT_CLUSTER
            || Services::get(cx)
                .core
                .snapshot()
                .clusters
                .iter()
                .any(|c| c.name == name);
        if taken {
            self.error = Some(format!("A cluster named `{name}` already exists.").into());
            cx.notify();
            return;
        }
        let mut plan = edit.plan.clone();
        plan.name = name.clone();

        window.close_dialog(cx);
        let _ = self
            .workspace
            .update(cx, |ws, cx| ws.select_cluster(name.clone(), cx));
        run_op(
            cx,
            format!("Couldn't create {name}"),
            move |core| async move { core.create_cluster(plan).await },
        )
        .detach();
    }

    #[cfg(feature = "debug-screenshots")]
    pub fn debug_plan_branch(&mut self, branch: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = SourceKind::Branch;
        self.branch_input.update(cx, |input, cx| {
            input.set_value(branch.to_string(), window, cx)
        });
        self.plan(window, cx);
    }

    #[cfg(feature = "debug-screenshots")]
    pub fn debug_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create(window, cx);
    }

    fn checkout_options(&self, project: &str, edit: &PlanEdit) -> Vec<(String, PlannedCheckout)> {
        let mut options = vec![(
            "Reuse the default cluster's instance".to_string(),
            PlannedCheckout::Reuse,
        )];
        if let Some(proposed) = edit.proposed.get(project)
            && *proposed != PlannedCheckout::Reuse
        {
            options.push((checkout_label(proposed), proposed.clone()));
        }
        for wt in self.external.iter().filter(|w| w.project == project) {
            let adopt = PlannedCheckout::Adopt {
                path: wt.path.clone(),
                branch: wt.branch.clone(),
            };
            if !options.iter().any(|(_, c)| *c == adopt) {
                options.push((checkout_label(&adopt), adopt));
            }
        }
        if let Some(current) = edit.plan.projects.get(project).map(|p| &p.checkout)
            && !options.iter().any(|(_, c)| c == current)
        {
            options.push((checkout_label(current), current.clone()));
        }
        options
    }

    fn set_checkout(&mut self, project: &str, checkout: PlannedCheckout, cx: &mut Context<Self>) {
        let default_db = self.db_defaults.get(project).copied();
        let Stage::Plan(edit) = &mut self.stage else {
            return;
        };
        if let Some(p) = edit.plan.projects.get_mut(project) {
            p.db = match (&checkout, default_db) {
                (PlannedCheckout::Reuse, _) | (_, None) => None,
                (_, Some(default)) => Some(p.db.unwrap_or(default)),
            };
            p.checkout = checkout;
        }
        cx.notify();
    }

    fn set_db(&mut self, project: &str, mode: DbMode, cx: &mut Context<Self>) {
        if let Stage::Plan(edit) = &mut self.stage
            && let Some(p) = edit.plan.projects.get_mut(project)
        {
            p.db = Some(mode);
            cx.notify();
        }
    }

    fn render_source(&self, cx: &Context<Self>) -> impl IntoElement {
        let is_pr = self.kind == SourceKind::PullRequest;
        let planning = matches!(self.stage, Stage::Planning);
        v_flex()
            .gap_4()
            .child(
                ButtonGroup::new("source-kind")
                    .outline()
                    .disabled(planning)
                    .child(
                        ui::icon_button("kind-pr", IconName::GitPullRequest, "Pull request", false)
                            .w(px(40.))
                            .selected(is_pr),
                    )
                    .child(
                        ui::icon_button("kind-branch", IconName::GitBranch, "Branch", false)
                            .w(px(40.))
                            .selected(!is_pr),
                    )
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                        this.kind = if clicked.contains(&1) {
                            SourceKind::Branch
                        } else {
                            SourceKind::PullRequest
                        };
                        this.error = None;
                        let input = match this.kind {
                            SourceKind::PullRequest => this.pr_input.clone(),
                            SourceKind::Branch => this.branch_input.clone(),
                        };
                        input.update(cx, |input, cx| input.focus(window, cx));
                        cx.notify();
                    })),
            )
            .child(if is_pr {
                Form::vertical()
                    .child(
                        Field::new()
                            .label("Pull request")
                            .child(Input::new(&self.pr_input).disabled(planning)),
                    )
                    .child(
                        Field::new()
                            .label("Project")
                            .description("Only needed for a bare PR number.")
                            .child(
                                Select::new(&self.project)
                                    .placeholder("From the URL")
                                    .cleanable(true)
                                    .disabled(planning),
                            ),
                    )
                    .into_any_element()
            } else {
                Form::vertical()
                    .child(
                        Field::new()
                            .label("Branch")
                            .description("Projects that have this branch get a worktree on it.")
                            .child(Input::new(&self.branch_input).disabled(planning)),
                    )
                    .child(
                        Field::new()
                            .label("Create it in")
                            .description("Creates the branch in this project if it doesn't exist.")
                            .child(
                                Select::new(&self.project)
                                    .placeholder("Only use existing branches")
                                    .cleanable(true)
                                    .disabled(planning),
                            ),
                    )
                    .child(
                        Field::new()
                            .label("Base")
                            .child(Input::new(&self.base_input).disabled(planning)),
                    )
                    .into_any_element()
            })
            .when(planning, |this| {
                this.child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(if is_pr {
                            "Looking up the pull request…"
                        } else {
                            "Fetching the branch…"
                        }),
                )
            })
    }

    fn render_plan(&self, edit: &PlanEdit, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows: Vec<AnyElement> = edit
            .plan
            .projects
            .iter()
            .enumerate()
            .map(|(ix, (project, pp))| {
                let options = self.checkout_options(project, edit);
                let selected = options.iter().position(|(_, c)| *c == pp.checkout);
                let checkouts: Vec<PlannedCheckout> =
                    options.iter().map(|(_, c)| c.clone()).collect();
                let has_db = self.db_defaults.contains_key(project);
                let reused = pp.checkout == PlannedCheckout::Reuse;
                let name = project.clone();
                let db_name = project.clone();
                let db = pp.db.or(self.db_defaults.get(project).copied());
                v_flex()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_semibold().child(project.clone()))
                            .when_some(pp.pr.as_ref(), |this, pr| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .min_w_0()
                                        .text_sm()
                                        .child(ui::inline_icon(IconName::GitPullRequest))
                                        .child(
                                            Link::new(("plan-pr", ix))
                                                .href(pr.url.clone())
                                                .child(format!("#{} {}", pr.number, pr.title)),
                                        ),
                                )
                            }),
                    )
                    .child(
                        RadioGroup::vertical(("checkout", ix))
                            .children(options.into_iter().map(|(label, _)| label))
                            .selected_index(selected)
                            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                                if let Some(checkout) = checkouts.get(*ix) {
                                    this.set_checkout(&name, checkout.clone(), cx);
                                }
                            })),
                    )
                    .when(has_db && !reused, |this| {
                        this.child(
                            h_flex()
                                .gap_3()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .text_color(theme.muted_foreground)
                                        .child(ui::inline_icon(IconName::Database))
                                        .child("Database"),
                                )
                                .child(
                                    ButtonGroup::new(("db", ix))
                                        .outline()
                                        .small()
                                        .child(
                                            Button::new(("db-shared", ix))
                                                .label("Shared")
                                                .selected(db == Some(DbMode::Shared)),
                                        )
                                        .child(
                                            Button::new(("db-fresh", ix))
                                                .label("Fresh copy")
                                                .selected(db == Some(DbMode::Fresh)),
                                        )
                                        .on_click(cx.listener(
                                            move |this, clicked: &Vec<usize>, _, cx| {
                                                let mode = if clicked.contains(&1) {
                                                    DbMode::Fresh
                                                } else {
                                                    DbMode::Shared
                                                };
                                                this.set_db(&db_name, mode, cx);
                                            },
                                        )),
                                ),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        v_flex()
            .gap_3()
            .child(
                Form::vertical().child(
                    Field::new()
                        .label("Name")
                        .description("Lowercase letters, digits and dashes; used in its URLs.")
                        .child(Input::new(&self.name_input)),
                ),
            )
            .child(
                v_flex()
                    .child(ui::section_title("Projects", cx))
                    .children(rows),
            )
            .when(!edit.plan.warnings.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .children(edit.plan.warnings.iter().map(|w| {
                            h_flex()
                                .gap_2()
                                .items_start()
                                .text_color(theme.warning)
                                .child(Icon::new(IconName::TriangleAlert).size(ui::ICON))
                                .child(div().flex_1().child(w.clone()))
                        })),
                )
            })
            .when(
                edit.plan
                    .projects
                    .values()
                    .any(|p| p.checkout == PlannedCheckout::Reuse)
                    && edit
                        .plan
                        .projects
                        .values()
                        .any(|p| p.db == Some(DbMode::Fresh)),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Reused projects keep using the default cluster's database."),
                    )
                },
            )
            .child(
                Checkbox::new("start-after-create")
                    .label("Start after creating")
                    .checked(edit.plan.start)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        if let Stage::Plan(edit) = &mut this.stage {
                            edit.plan.start = *checked;
                            cx.notify();
                        }
                    })),
            )
    }
}

impl Render for NewClusterDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.stage {
            Stage::Plan(edit) => self.render_plan(edit, cx).into_any_element(),
            _ => self.render_source(cx).into_any_element(),
        };
        v_flex()
            .gap_3()
            .pb_1()
            .child(body)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .text_color(cx.theme().danger)
                        .child(Icon::new(IconName::CircleAlert).size(ui::ICON))
                        .child(div().flex_1().child(error)),
                )
            })
    }
}

fn checkout_label(checkout: &PlannedCheckout) -> String {
    match checkout {
        PlannedCheckout::Reuse => "Reuse the default cluster's instance".into(),
        PlannedCheckout::Worktree { source } => match source {
            WorktreeSource::Branch { name } => format!("New worktree on {name}"),
            WorktreeSource::NewBranch { name, base } => {
                format!("New worktree on a new branch {name}, from {base}")
            }
            WorktreeSource::Fetch { local_branch, .. } => {
                format!("New worktree on {local_branch}")
            }
        },
        PlannedCheckout::Adopt { path, branch } => format!(
            "Existing worktree at {}{}",
            ui::tilde(path),
            branch
                .as_ref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default()
        ),
    }
}
