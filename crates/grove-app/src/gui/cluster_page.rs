//! The selected cluster: header with actions, one section per project, and
//! recent activity.

use super::services::{MainWindow, run_op};
use super::ui::{self, no_drag};
use super::workspace::Workspace;
use gpui_kit::assets::IconName;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::link::Link;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Context, Hsla, IntoElement, SharedString, div};
use grove_core::{
    ActivityState, ActivityView, CheckoutView, ClusterOrigin, ClusterPhase, ClusterView, DbMode,
    InstanceView, ProcessView, Snapshot,
};
use grove_github::PrState;

const MAX_ACTIVITIES: usize = 20;

impl Workspace {
    /// The selected cluster's toolbar content and scrolling page.
    pub(super) fn render_cluster_page(
        &self,
        snapshot: &Snapshot,
        cx: &Context<Self>,
    ) -> (Option<AnyElement>, AnyElement) {
        let Some(cluster) = snapshot
            .cluster(&self.selected)
            .or_else(|| snapshot.cluster(grove_config::DEFAULT_CLUSTER))
        else {
            return (None, self.render_empty(cx).into_any_element());
        };

        let (toolbar, summary) = self.render_header(cluster, cx);
        let page = v_flex()
            .id("cluster-page")
            .size_full()
            .child(summary)
            .child(self.render_projects(cluster, cx))
            .child(self.render_activities(cluster, cx))
            .overflow_y_scrollbar()
            .into_any_element();
        (Some(toolbar), page)
    }

    fn render_empty(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(div().child("No clusters yet."))
    }

    /// The toolbar (name, state, actions) and the summary under it.
    fn render_header(&self, c: &ClusterView, cx: &Context<Self>) -> (AnyElement, AnyElement) {
        let theme = cx.theme();
        let name = c.name.clone();
        let busy = matches!(
            c.phase,
            ClusterPhase::Creating { .. } | ClusterPhase::TearingDown | ClusterPhase::Error { .. }
        );
        let running = c.phase.is_active();
        let first_url = c.urls().into_iter().next().map(|(_, url)| url);
        let start_key = format!("start:{name}");
        let stop_key = format!("stop:{name}");
        let restart_key = format!("restart:{name}");
        let any_pending = self.is_pending(&start_key)
            || self.is_pending(&stop_key)
            || self.is_pending(&restart_key);

        let origin = match &c.origin {
            ClusterOrigin::PullRequest {
                project,
                number,
                url,
            } => h_flex()
                .gap_1()
                .child(ui::inline_icon(IconName::GitPullRequest))
                .child(
                    Link::new("origin-pr")
                        .href(url.clone())
                        .child(format!("PR #{number}")),
                )
                .child(format!("in {project}"))
                .into_any_element(),
            ClusterOrigin::Branch { branch } => h_flex()
                .gap_1()
                .child(ui::inline_icon(IconName::GitBranch))
                .child(branch.clone())
                .into_any_element(),
            ClusterOrigin::Default => div()
                .child("Your main clones, on whatever branch they have checked out")
                .into_any_element(),
            ClusterOrigin::Manual => div().child("Custom cluster").into_any_element(),
        };

        let lifecycle = ui::capsule(cx)
            .child(
                ui::toolbar_button(
                    "cluster-start",
                    IconName::Play,
                    "Start",
                    self.is_pending(&start_key),
                )
                .disabled(busy || running || any_pending)
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, _, cx| {
                        let n = name.clone();
                        this.run_tracked(
                            format!("start:{n}"),
                            format!("Couldn't start {n}"),
                            move |core| async move { core.start_cluster(&n).await },
                            |_, _, _| {},
                            cx,
                        );
                    }
                })),
            )
            .child(
                ui::toolbar_button(
                    "cluster-stop",
                    IconName::Square,
                    "Stop",
                    self.is_pending(&stop_key),
                )
                .disabled(busy || !running || any_pending)
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, _, cx| {
                        let n = name.clone();
                        this.run_tracked(
                            format!("stop:{n}"),
                            format!("Couldn't stop {n}"),
                            move |core| async move { core.stop_cluster(&n).await },
                            |_, _, _| {},
                            cx,
                        );
                    }
                })),
            )
            .child(
                ui::toolbar_button(
                    "cluster-restart",
                    IconName::RotateCw,
                    "Restart",
                    self.is_pending(&restart_key),
                )
                .disabled(busy || !running || any_pending)
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, _, cx| {
                        let n = name.clone();
                        this.run_tracked(
                            format!("restart:{n}"),
                            format!("Couldn't restart {n}"),
                            move |core| async move { core.restart_cluster(&n).await },
                            |_, _, _| {},
                            cx,
                        );
                    }
                })),
            );

        let open_tooltip = match &first_url {
            Some(url) => format!("Open {url}"),
            None => "Open in browser".to_string(),
        };
        let destinations = ui::capsule(cx)
            .child(
                ui::toolbar_button("cluster-open", IconName::ExternalLink, open_tooltip, false)
                    .disabled(first_url.is_none())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Some(url) = &first_url {
                            cx.open_url(url);
                        }
                    })),
            )
            .when(!c.is_default, |this| {
                let name = name.clone();
                this.child(
                    ui::toolbar_button("cluster-teardown", IconName::Trash, "Tear down…", false)
                        .disabled(matches!(
                            c.phase,
                            ClusterPhase::Creating { .. } | ClusterPhase::TearingDown
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_teardown(name.clone(), window, cx)
                        })),
                )
            });

        let new_cluster = ui::capsule(cx).child(
            ui::toolbar_button(
                "toolbar-new-cluster",
                IconName::Plus,
                "New cluster… (⌘N)",
                false,
            )
            .on_click(cx.listener(|this, _, window, cx| this.open_new_cluster(window, cx))),
        );

        let toolbar = h_flex()
            .flex_1()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .truncate()
                    .child(c.name.clone()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(ui::dot(ui::phase_color(&c.phase, cx)))
                    .child(c.phase.label().to_string()),
            )
            .child(div().flex_1())
            .child(
                no_drag(h_flex().flex_none().gap_2())
                    .child(lifecycle)
                    .child(destinations)
                    .child(new_cluster),
            )
            .into_any_element();

        let summary = v_flex()
            .px_6()
            .pt_3()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(origin),
            )
            .when_some(
                match &c.phase {
                    ClusterPhase::Creating { step } => Some(step.clone()),
                    _ => None,
                },
                |this, step| {
                    this.child(
                        h_flex()
                            .gap_2()
                            .text_color(theme.muted_foreground)
                            .child(Spinner::new().with_size(Size::Size(ui::ICON)))
                            .child(format!("Creating: {step}")),
                    )
                },
            )
            .when_some(
                match &c.phase {
                    ClusterPhase::Error { message } => Some(message.clone()),
                    _ => None,
                },
                |this, message| {
                    let retry_key = format!("retry:{name}");
                    let n = name.clone();
                    this.child(
                        v_flex()
                            .gap_2()
                            .child(
                                Alert::error("cluster-error", message)
                                    .title("Creating this cluster failed"),
                            )
                            .child(
                                h_flex().gap_2().child(
                                    Button::new("cluster-retry")
                                        .label("Retry")
                                        .loading(self.is_pending(&retry_key))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let n = n.clone();
                                            this.run_tracked(
                                                format!("retry:{n}"),
                                                format!("Couldn't create {n}"),
                                                move |core| async move {
                                                    core.retry_cluster(&n, true).await
                                                },
                                                |_, _, _| {},
                                                cx,
                                            );
                                        })),
                                ),
                            ),
                    )
                },
            )
            .into_any_element();
        (toolbar, summary)
    }

    fn render_projects(&self, c: &ClusterView, cx: &Context<Self>) -> impl IntoElement {
        let count = c.instances.len();
        let mut rows = Vec::with_capacity(count);
        for (ix, inst) in c.instances.iter().enumerate() {
            rows.push(self.render_instance(c, inst, ix + 1 == count, cx));
        }
        v_flex()
            .px_6()
            .pt_4()
            .gap_1()
            .child(ui::section_title("Projects", cx))
            .children(rows)
    }

    fn render_instance(
        &self,
        c: &ClusterView,
        inst: &InstanceView,
        last: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let cluster = c.name.clone();
        let project = inst.project.clone();
        let reused = inst.checkout == CheckoutView::Reuse;
        let busy = matches!(
            c.phase,
            ClusterPhase::Creating { .. } | ClusterPhase::TearingDown
        );

        let checkout: AnyElement = match &inst.checkout {
            CheckoutView::MainClone { exists: true, .. } => {
                let branch = inst.git.as_ref().and_then(|g| g.branch.clone());
                chip(
                    IconName::GitBranch,
                    branch.unwrap_or_else(|| "main clone".into()),
                    cx,
                )
            }
            CheckoutView::MainClone { exists: false, .. } => div()
                .text_sm()
                .text_color(theme.danger)
                .child("Main clone missing")
                .into_any_element(),
            CheckoutView::Worktree {
                branch, adopted, ..
            } => h_flex()
                .gap_1()
                .child(chip(
                    IconName::GitBranch,
                    branch.clone().unwrap_or_else(|| "detached".into()),
                    cx,
                ))
                .when(*adopted, |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("adopted worktree"),
                    )
                })
                .into_any_element(),
            CheckoutView::Reuse => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Reuses the default cluster's instance")
                .into_any_element(),
        };

        let actions = h_flex()
            .flex_none()
            .gap_0p5()
            .when(!reused, |this| {
                let pull_key = format!("pull:{cluster}:{project}");
                let setup_key = format!("setup:{cluster}:{project}");
                this.child(
                    ui::icon_button(
                        SharedString::from(format!("pull-{project}")),
                        IconName::ArrowDownToLine,
                        "Pull: git pull --ff-only, then after-pull hooks",
                        self.is_pending(&pull_key),
                    )
                    .disabled(busy)
                    .on_click(cx.listener({
                        let (c, p) = (cluster.clone(), project.clone());
                        move |this, _, _, cx| this.pull(c.clone(), p.clone(), cx)
                    })),
                )
                .child(
                    ui::icon_button(
                        SharedString::from(format!("editor-{project}")),
                        IconName::Code,
                        "Editor",
                        false,
                    )
                    .on_click(cx.listener({
                        let (c, p) = (cluster.clone(), project.clone());
                        move |_, _, _, cx| {
                            let (c, p) = (c.clone(), p.clone());
                            run_op(cx, format!("Couldn't open {p}"), move |core| async move {
                                core.open_in_editor(&c, &p).await
                            })
                            .detach();
                        }
                    })),
                )
                .child(
                    ui::icon_button(
                        SharedString::from(format!("shell-{project}")),
                        IconName::SquareTerminal,
                        "Shell",
                        false,
                    )
                    .on_click(cx.listener({
                        let (c, p) = (cluster.clone(), project.clone());
                        move |this, _, window, cx| {
                            this.output
                                .update(cx, |panel, cx| panel.open_shell(&c, &p, window, cx));
                        }
                    })),
                )
                .child(
                    ui::icon_button(
                        SharedString::from(format!("setup-{project}")),
                        IconName::Wrench,
                        "Re-run setup",
                        self.is_pending(&setup_key),
                    )
                    .disabled(busy)
                    .on_click(cx.listener({
                        let (c, p) = (cluster.clone(), project.clone());
                        move |this, _, _, cx| {
                            let (c, p) = (c.clone(), p.clone());
                            this.run_tracked(
                                format!("setup:{c}:{p}"),
                                format!("Setup of {p} failed"),
                                move |core| async move { core.rerun_setup(&c, &p).await },
                                |_, _, _| {},
                                cx,
                            );
                        }
                    })),
                )
            })
            .when(reused && !c.is_default, |this| {
                this.child(
                    Button::new(SharedString::from(format!("show-default-{project}")))
                        .ghost()
                        .small()
                        .label("Show default")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.select_cluster(grove_config::DEFAULT_CLUSTER.into(), cx)
                        })),
                )
            });

        v_flex()
            .py_3()
            .gap_2()
            .when(!last, |this| this.border_b_1().border_color(theme.border))
            .child(
                h_flex()
                    .justify_between()
                    .gap_3()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(div().font_semibold().child(inst.project.clone()))
                            .child(checkout),
                    )
                    .child(actions),
            )
            .when(!reused, |this| this.child(self.render_meta(inst, cx)))
            .when(!inst.processes.is_empty(), |this| {
                this.child(
                    v_flex().children(
                        inst.processes
                            .iter()
                            .map(|p| self.render_process(c, inst, p, busy, cx)),
                    ),
                )
            })
    }

    /// Git status, PR, database and path of a checkout.
    fn render_meta(&self, inst: &InstanceView, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut items: Vec<AnyElement> = Vec::new();

        if let Some(git) = &inst.git {
            let dirty = git.changes.len();
            items.push(if dirty > 0 {
                div()
                    .text_color(theme.warning)
                    .child(format!(
                        "{dirty} uncommitted {}",
                        if dirty == 1 { "change" } else { "changes" }
                    ))
                    .into_any_element()
            } else {
                div().child("clean").into_any_element()
            });
            if git.ahead > 0 || git.behind > 0 {
                items.push(
                    div()
                        .child(format!("↑{} ↓{}", git.ahead, git.behind))
                        .into_any_element(),
                );
            } else if git.upstream.is_none() && git.branch.is_some() {
                items.push(div().child("no upstream").into_any_element());
            }
        }
        if let Some(pr) = &inst.pr {
            let (state, color) = match pr.state {
                Some(PrState::Open) => ("open", theme.success),
                Some(PrState::Merged) => ("merged", theme.info),
                Some(PrState::Closed) => ("closed", theme.danger),
                None => ("", theme.muted_foreground),
            };
            items.push(
                h_flex()
                    .gap_1()
                    .min_w_0()
                    .child(ui::inline_icon(IconName::GitPullRequest))
                    .child(
                        Link::new(SharedString::from(format!("pr-{}", inst.project)))
                            .href(pr.url.clone())
                            .child(format!("#{} {}", pr.number, pr.title)),
                    )
                    .when(!state.is_empty(), |this| {
                        this.child(div().text_color(color).child(state))
                    })
                    .into_any_element(),
            );
        }
        if let Some(db) = &inst.database {
            let mode = match db.mode {
                DbMode::Shared => "shared",
                DbMode::Fresh => "fresh",
            };
            items.push(
                h_flex()
                    .gap_1()
                    .child(ui::inline_icon(IconName::Database))
                    .child(format!("{} ({mode})", db.name))
                    .into_any_element(),
            );
        }
        if let Some(path) = inst.checkout.path() {
            items.push(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .truncate()
                    .child(ui::tilde(path))
                    .into_any_element(),
            );
        }

        h_flex()
            .flex_wrap()
            .gap_x_4()
            .gap_y_1()
            .text_sm()
            .text_color(theme.muted_foreground)
            .children(items)
    }

    fn render_process(
        &self,
        c: &ClusterView,
        inst: &InstanceView,
        p: &ProcessView,
        busy: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (cluster, project, process) = (c.name.clone(), inst.project.clone(), p.name.clone());
        let key = format!("{cluster}/{project}/{process}");
        let running = p.state.is_running() || p.state == grove_core::ProcState::Waiting;
        let pending = self.is_pending(&format!("proc:{key}"));
        let id = |what: &str| SharedString::from(format!("{what}-{key}"));

        let op = |verb: &'static str| {
            let (c, pr, n) = (cluster.clone(), project.clone(), process.clone());
            cx.listener(move |this: &mut Workspace, _, _, cx| {
                let (c, pr, n) = (c.clone(), pr.clone(), n.clone());
                this.run_tracked(
                    format!("proc:{c}/{pr}/{n}"),
                    format!("Couldn't {verb} {pr}.{n}"),
                    move |core| async move {
                        match verb {
                            "start" => core.start_process(&c, &pr, &n).await,
                            "stop" => core.stop_process(&c, &pr, &n).await,
                            _ => core.restart_process(&c, &pr, &n).await,
                        }
                    },
                    |_, _, _| {},
                    cx,
                );
            })
        };

        h_flex()
            .id(id("proc-row"))
            .gap_3()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|style| style.bg(theme.muted.opacity(0.5)))
            .child(
                h_flex()
                    .w_48()
                    .flex_none()
                    .gap_2()
                    .child(ui::dot(ui::proc_color(&p.state, cx)))
                    .child(div().font_medium().truncate().child(p.name.clone())),
            )
            .child(
                div()
                    .w_40()
                    .flex_none()
                    .text_sm()
                    .text_color(state_color(&p.state, cx))
                    .truncate()
                    .child(ui::proc_state_text(&p.state)),
            )
            .child(
                div()
                    .w_12()
                    .flex_none()
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(p.port.map(|port| format!(":{port}")).unwrap_or_default()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .when_some(p.url.clone(), |this, url| {
                        this.child(Link::new(id("url")).href(url.clone()).child(url))
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .child(if running {
                        ui::icon_button(id("stop"), IconName::Square, "Stop", pending)
                            .disabled(busy)
                            .on_click(op("stop"))
                    } else {
                        ui::icon_button(id("start"), IconName::Play, "Start", pending)
                            .disabled(busy)
                            .on_click(op("start"))
                    })
                    .child(
                        ui::icon_button(id("restart"), IconName::RotateCw, "Restart", false)
                            .disabled(busy || pending)
                            .on_click(op("restart")),
                    )
                    .child(
                        ui::icon_button(id("output"), IconName::SquareTerminal, "Output", false)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.output.update(cx, |panel, cx| {
                                    panel.open_process(&cluster, &project, &process, window, cx)
                                });
                            })),
                    ),
            )
    }

    fn render_activities(&self, c: &ClusterView, cx: &Context<Self>) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for a in c.activities.iter().rev().take(MAX_ACTIVITIES) {
            rows.push(self.render_activity(&c.name, a, cx).into_any_element());
        }
        let theme = cx.theme();
        v_flex()
            .px_6()
            .pt_4()
            .pb_6()
            .gap_1()
            .child(ui::section_title("Recent activity", cx))
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .py_2()
                        .text_color(theme.muted_foreground)
                        .child("Setup steps and git operations will show up here."),
                )
            })
            .children(rows)
    }

    fn render_activity(
        &self,
        cluster: &str,
        a: &ActivityView,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let icon: AnyElement = match &a.state {
            ActivityState::Running => Spinner::new()
                .with_size(Size::Size(ui::ICON))
                .into_any_element(),
            ActivityState::Succeeded => Icon::new(IconName::Check)
                .size(ui::ICON)
                .text_color(ui::activity_color(&a.state, cx))
                .into_any_element(),
            ActivityState::Failed { .. } => Icon::new(IconName::X)
                .size(ui::ICON)
                .text_color(ui::activity_color(&a.state, cx))
                .into_any_element(),
        };
        let (cluster, id, title) = (cluster.to_string(), a.id, a.title.clone());
        v_flex()
            .id(("activity", a.id))
            .px_2()
            .py_1()
            .gap_0p5()
            .rounded(theme.radius)
            .hover(|style| style.bg(theme.muted.opacity(0.5)))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_none().w_4().child(icon))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_family(theme.mono_font_family.clone())
                            .truncate()
                            .child(a.title.clone()),
                    )
                    .when_some(a.project.clone(), |this, project| {
                        this.child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(project),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w_16()
                            .text_sm()
                            .text_right()
                            .text_color(theme.muted_foreground)
                            .child(ui::ago(a.started_at)),
                    ),
            )
            .when_some(
                match &a.state {
                    ActivityState::Failed { message } => Some(message.clone()),
                    _ => None,
                },
                |this, message| {
                    this.child(
                        div()
                            .pl_6()
                            .text_sm()
                            .text_color(theme.danger)
                            .child(message),
                    )
                },
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.output.update(cx, |panel, cx| {
                    panel.open_activity(&cluster, id, &title, window, cx)
                });
            }))
    }

    /// `git pull --ff-only` + after_pull, then offers to restart the
    /// project's processes.
    fn pull(&mut self, cluster: String, project: String, cx: &mut Context<Self>) {
        let processes: Vec<String> = self
            .snapshot(cx)
            .cluster(&cluster)
            .and_then(|c| c.instance(&project))
            .map(|i| i.processes.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        let (c, p) = (cluster.clone(), project.clone());
        self.run_tracked(
            format!("pull:{cluster}:{project}"),
            format!("Couldn't pull {project}"),
            move |core| async move { core.pull(&c, &p).await },
            move |_, output, cx| {
                let Some(output) = output else { return };
                let summary = output
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("Pulled")
                    .to_string();
                pulled_notification(cluster, project, summary, processes, cx);
            },
            cx,
        );
    }
}

fn pulled_notification(
    cluster: String,
    project: String,
    summary: String,
    processes: Vec<String>,
    cx: &mut App,
) {
    let Some(handle) = MainWindow::handle(cx) else {
        return;
    };
    let mut note = Notification::success(summary).title(format!("Pulled {project}"));
    if !processes.is_empty() {
        note = note.action(move |_, _, cx| {
            let (cluster, project, processes) =
                (cluster.clone(), project.clone(), processes.clone());
            Button::new("restart-after-pull")
                .small()
                .label("Restart processes")
                .on_click(cx.listener(move |note, _, window, cx| {
                    for process in &processes {
                        let (c, p, n) = (cluster.clone(), project.clone(), process.clone());
                        run_op(
                            cx,
                            format!("Couldn't restart {p}.{n}"),
                            move |core| async move { core.restart_process(&c, &p, &n).await },
                        )
                        .detach();
                    }
                    note.dismiss(window, cx);
                }))
        });
    }
    let _ = handle.update(cx, |_, window, cx| window.push_notification(note, cx));
}

/// A small icon + monospace label, e.g. a branch.
fn chip(icon: IconName, text: String, cx: &App) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .gap_1()
        .px_1p5()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(ui::inline_icon(icon))
        .child(
            div()
                .font_family(theme.mono_font_family.clone())
                .truncate()
                .child(text),
        )
        .into_any_element()
}

fn state_color(state: &grove_core::ProcState, cx: &App) -> Hsla {
    match state {
        grove_core::ProcState::Exited { success: false, .. }
        | grove_core::ProcState::Failed { .. } => cx.theme().danger,
        _ => cx.theme().muted_foreground,
    }
}
