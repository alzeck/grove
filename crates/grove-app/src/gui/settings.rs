//! Settings (a sheet): user config, projects, certificate, diagnostics.

use super::services::{Services, notify, run_bg, run_op};
use super::{theme, ui};
use gpui_kit::assets::IconName;
use gpui_kit::component::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Task, WeakEntity, Window,
    div,
};
use grove_config::Appearance;
use grove_core::{Core, DoctorCheck, NoticeLevel, UserConfigDraft};
use std::collections::HashSet;
use std::path::PathBuf;

/// A workspace project, as onboarding and settings list them.
#[derive(Clone)]
pub struct ProjectRow {
    pub name: String,
    pub repo: String,
    pub main_clone: PathBuf,
    pub cloned: bool,
    pub enabled: bool,
}

/// Every project in the workspace, or why they can't be listed.
pub fn load_projects(core: &Core) -> Result<Vec<ProjectRow>, String> {
    core.workspace_projects()
        .map(|list| {
            list.into_iter()
                .map(|p| ProjectRow {
                    name: p.name,
                    repo: p.repo,
                    main_clone: p.main_clone,
                    cloned: p.cloned,
                    enabled: p.enabled,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// Whether the login keychain trusts Grove's CA (`None` if there's no CA).
pub fn check_trusted(cx: &mut App) -> Task<Option<bool>> {
    let ca = Services::get(cx).core.ca();
    let job = run_bg(cx, async move {
        let ca = ca?;
        tokio::task::spawn_blocking(move || ca.is_trusted())
            .await
            .ok()
    });
    cx.spawn(async move |_| job.await.ok().flatten())
}

/// Adds Grove's CA to the login keychain; macOS asks for the password.
pub fn install_trust(cx: &mut App) -> Task<Result<(), String>> {
    let ca = Services::get(cx).core.ca();
    let job = run_bg(cx, async move {
        let Some(ca) = ca else {
            return Err("Grove has no certificate authority".to_string());
        };
        tokio::task::spawn_blocking(move || ca.install_trust().map_err(|e| e.to_string()))
            .await
            .map_err(|e| e.to_string())?
    });
    cx.spawn(async move |_| job.await.and_then(|r| r))
}

pub struct SettingsView {
    appearance: Appearance,
    workspace: Entity<InputState>,
    clones_root: Entity<InputState>,
    editor: Entity<InputState>,
    postgres_url: Entity<InputState>,
    idle_timeout: Entity<InputState>,
    /// `projects = None` in the file: every project enabled.
    all_projects: bool,
    projects: Result<Vec<ProjectRow>, String>,
    enabled: HashSet<String>,
    cloning: HashSet<String>,
    doctor: Option<Vec<DoctorCheck>>,
    trusted: Option<bool>,
    trusting: bool,
    saving: bool,
    syncing: bool,
    error: Option<SharedString>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let core = Services::get(cx).core;
        let draft = core.user_config_draft();
        let input = |value: Option<String>,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.unwrap_or_default())
            })
        };
        let workspace = input(
            Some(draft.workspace.clone()),
            "git@github.com:acme/grove-workspace.git or ~/path",
            window,
            cx,
        );
        let clones_root = input(Some(draft.clones_root.clone()), "~/Developer", window, cx);
        let editor = input(
            draft.editor.clone(),
            "Detected (Zed, Cursor, VS Code)",
            window,
            cx,
        );
        let postgres_url = input(
            draft.postgres_url.clone(),
            "From the workspace (e.g. postgres://localhost:5432)",
            window,
            cx,
        );
        let idle_timeout = input(
            draft.idle_timeout.clone(),
            "From the workspace (e.g. 30m)",
            window,
            cx,
        );

        let mut this = Self {
            appearance: theme::appearance(cx),
            workspace,
            clones_root,
            editor,
            postgres_url,
            idle_timeout,
            all_projects: draft.projects.is_none(),
            projects: Ok(Vec::new()),
            enabled: HashSet::new(),
            cloning: HashSet::new(),
            doctor: None,
            trusted: None,
            trusting: false,
            saving: false,
            syncing: false,
            error: None,
        };
        this.reload_projects(cx);
        this.refresh_trust(cx);
        this.run_doctor(cx);
        this
    }

    fn reload_projects(&mut self, cx: &mut Context<Self>) {
        let core = Services::get(cx).core;
        self.projects = load_projects(&core);
        if let Ok(projects) = &self.projects {
            self.enabled = projects
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.name.clone())
                .collect();
        }
        cx.notify();
    }

    fn refresh_trust(&mut self, cx: &mut Context<Self>) {
        let task = check_trusted(cx);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let trusted = task.await;
            let _ = this.update(cx, |this, cx| {
                this.trusted = trusted;
                cx.notify();
            });
        })
        .detach();
    }

    fn run_doctor(&mut self, cx: &mut Context<Self>) {
        self.doctor = None;
        let core = Services::get(cx).core;
        let job = run_bg(cx, async move { core.doctor().await });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let checks = job.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.doctor = Some(checks);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn draft(&self, cx: &App) -> UserConfigDraft {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let optional = |input: &Entity<InputState>| Some(text(input)).filter(|v| !v.is_empty());
        let projects = match &self.projects {
            Ok(list) if !list.is_empty() => {
                let all = list.iter().all(|p| self.enabled.contains(&p.name));
                if all && self.all_projects {
                    None
                } else {
                    Some(
                        list.iter()
                            .filter(|p| self.enabled.contains(&p.name))
                            .map(|p| p.name.clone())
                            .collect(),
                    )
                }
            }
            _ => Services::get(cx).core.user_config_draft().projects,
        };
        UserConfigDraft {
            workspace: text(&self.workspace),
            clones_root: text(&self.clones_root),
            projects,
            editor: optional(&self.editor),
            postgres_url: optional(&self.postgres_url),
            idle_timeout: optional(&self.idle_timeout),
            appearance: self.appearance,
        }
    }

    /// Applies the appearance right away and saves just that setting, so
    /// unsaved edits in the other fields stay unsaved.
    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        if self.appearance == appearance {
            return;
        }
        self.appearance = appearance;
        theme::set_appearance(appearance, cx);
        let core = Services::get(cx).core;
        let mut stored = core.user_config_draft();
        stored.appearance = appearance;
        let task = run_bg(cx, async move {
            core.write_user_config(&stored).map_err(|e| e.to_string())
        });
        cx.spawn(async move |_, cx| {
            if let Err(e) = task.await.and_then(|r| r) {
                cx.update(|cx| {
                    notify(
                        cx,
                        NoticeLevel::Error,
                        format!("Couldn't save the appearance: {e}"),
                    )
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        self.saving = true;
        self.error = None;
        cx.notify();
        let core = Services::get(cx).core;
        let task = run_bg(cx, async move {
            core.write_user_config(&draft).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await.and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.all_projects = this.draft(cx).projects.is_none();
                        this.reload_projects(cx);
                        this.run_doctor(cx);
                        notify(cx, NoticeLevel::Info, "Settings saved");
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_workspace(&mut self, cx: &mut Context<Self>) {
        self.syncing = true;
        cx.notify();
        let task = run_op(cx, "Couldn't update the workspace", |core| async move {
            core.sync_workspace().await
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let message = task.await;
            let _ = this.update(cx, |this, cx| {
                this.syncing = false;
                if let Some(message) = message {
                    notify(cx, NoticeLevel::Info, capitalize(&message));
                }
                this.reload_projects(cx);
            });
        })
        .detach();
    }

    fn reload_config(&mut self, cx: &mut Context<Self>) {
        let status = Services::get(cx).core.reload_config();
        let (level, message) = match status {
            grove_core::ConfigStatus::Loaded { warnings, .. } if warnings.is_empty() => {
                (NoticeLevel::Info, "Config reloaded".to_string())
            }
            grove_core::ConfigStatus::Loaded { warnings, .. } => (
                NoticeLevel::Warning,
                format!("Config reloaded with {} warning(s)", warnings.len()),
            ),
            grove_core::ConfigStatus::Invalid { diagnostics } => (
                NoticeLevel::Error,
                format!("Config has {} problem(s)", diagnostics.len()),
            ),
            grove_core::ConfigStatus::NotConfigured => {
                (NoticeLevel::Warning, "Grove isn't configured".to_string())
            }
            grove_core::ConfigStatus::WorkspaceMissing { message } => (NoticeLevel::Error, message),
        };
        notify(cx, level, message);
        self.reload_projects(cx);
        self.run_doctor(cx);
    }

    fn trust(&mut self, cx: &mut Context<Self>) {
        self.trusting = true;
        cx.notify();
        let task = install_trust(cx);
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.trusting = false;
                if let Err(e) = result {
                    notify(
                        cx,
                        NoticeLevel::Error,
                        format!("Couldn't trust the certificate: {e}"),
                    );
                }
                this.refresh_trust(cx);
                this.run_doctor(cx);
            });
        })
        .detach();
    }

    fn clone_project(&mut self, name: String, cx: &mut Context<Self>) {
        if !self.cloning.insert(name.clone()) {
            return;
        }
        cx.notify();
        let n = name.clone();
        let task = run_op(
            cx,
            format!("Couldn't clone {name}"),
            move |core| async move { core.clone_project(&n).await },
        );
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let _ = task.await;
            let _ = this.update(cx, |this, cx| {
                this.cloning.remove(&name);
                this.reload_projects(cx);
            });
        })
        .detach();
    }

    fn render_projects(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match &self.projects {
            Err(e) => div()
                .text_color(theme.muted_foreground)
                .child(format!("Projects appear once the workspace loads ({e})."))
                .into_any_element(),
            Ok(list) if list.is_empty() => div()
                .text_color(theme.muted_foreground)
                .child("The workspace has no projects.")
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
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let trust_row = h_flex()
            .gap_3()
            .justify_between()
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
                        Some(true) => "Trusted by macOS",
                        Some(false) => "Not trusted yet: browsers will warn",
                        None => "Checking…",
                    }),
            )
            .when(self.trusted == Some(false), |this| {
                this.child(
                    Button::new("trust-ca")
                        .small()
                        .label("Trust certificate…")
                        .loading(self.trusting)
                        .on_click(cx.listener(|this, _, _, cx| this.trust(cx))),
                )
            });

        v_flex()
            .gap_6()
            .pb_4()
            .child(
                v_flex()
                    .gap_3()
                    .child(ui::section_title("General", cx))
                    .child(
                        Form::vertical()
                            .child(
                                Field::new().label("Appearance").child(
                                    TabBar::new("appearance")
                                        .segmented()
                                        .child(Tab::new().label("System"))
                                        .child(Tab::new().label("Light"))
                                        .child(Tab::new().label("Dark"))
                                        .selected_index(match self.appearance {
                                            Appearance::System => 0,
                                            Appearance::Light => 1,
                                            Appearance::Dark => 2,
                                        })
                                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                            let appearance = match ix {
                                                1 => Appearance::Light,
                                                2 => Appearance::Dark,
                                                _ => Appearance::System,
                                            };
                                            this.set_appearance(appearance, cx);
                                        })),
                                ),
                            )
                            .child(
                                Field::new()
                                    .label("Workspace")
                                    .description(
                                        "A local path or a git URL (cloned to ~/.grove/workspace).",
                                    )
                                    .child(Input::new(&self.workspace)),
                            )
                            .child(
                                Field::new()
                                    .label("Clones root")
                                    .description("Main clones live at <root>/<project>.")
                                    .child(Input::new(&self.clones_root)),
                            )
                            .child(
                                Field::new()
                                    .label("Editor")
                                    .description("Command with {{ path }}, e.g. zed {{ path }}.")
                                    .child(Input::new(&self.editor)),
                            )
                            .child(
                                Field::new()
                                    .label("Postgres URL")
                                    .child(Input::new(&self.postgres_url)),
                            )
                            .child(
                                Field::new()
                                    .label("Idle timeout")
                                    .description(
                                        "Stop clusters nobody used for this long; 0 disables.",
                                    )
                                    .child(Input::new(&self.idle_timeout)),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title("Projects on this machine", cx))
                    .child(self.render_projects(cx)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_color(theme.danger).child(error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-settings")
                            .primary()
                            .label("Save")
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        ui::icon_button(
                            "sync-workspace",
                            IconName::RefreshCw,
                            "Update workspace",
                            self.syncing,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.sync_workspace(cx))),
                    )
                    .child(
                        Button::new("reload-config")
                            .ghost()
                            .label("Reload config")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_config(cx))),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title("HTTPS certificate", cx))
                    .child(trust_row),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(ui::section_title("Diagnostics", cx))
                            .child(
                                ui::icon_button(
                                    "run-doctor",
                                    IconName::RefreshCw,
                                    "Check again",
                                    false,
                                )
                                .disabled(self.doctor.is_none())
                                .on_click(cx.listener(|this, _, _, cx| this.run_doctor(cx))),
                            ),
                    )
                    .child(render_checks(self.doctor.as_deref(), cx)),
            )
    }
}

/// One project with an enable checkbox and a clone button if it's missing.
pub fn project_row(
    p: &ProjectRow,
    enabled: bool,
    cloning: bool,
    on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    on_clone: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .gap_3()
        .py_1()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    Checkbox::new(SharedString::from(format!("enable-{}", p.name)))
                        .checked(enabled)
                        .on_change(on_toggle),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().font_medium().child(p.name.clone()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(if p.cloned {
                                    ui::tilde(&p.main_clone)
                                } else {
                                    format!("Not cloned yet · {}", p.repo)
                                }),
                        ),
                ),
        )
        .when(!p.cloned, |this| {
            this.child(
                ui::icon_button(
                    SharedString::from(format!("clone-{}", p.name)),
                    IconName::Download,
                    "Clone",
                    cloning,
                )
                .on_click(on_clone),
            )
        })
}

/// Diagnostics: ✓/✗ per check with its detail.
pub fn render_checks(checks: Option<&[DoctorCheck]>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let Some(checks) = checks else {
        return h_flex()
            .gap_2()
            .text_color(theme.muted_foreground)
            .child(Spinner::new().with_size(Size::Size(ui::ICON)))
            .child("Checking…")
            .into_any_element();
    };
    v_flex()
        .gap_1p5()
        .children(checks.iter().map(|c| {
            h_flex()
                .gap_2()
                .items_start()
                .child(div().pt_0p5().child(if c.ok {
                    Icon::new(IconName::Check)
                        .size(ui::ICON)
                        .text_color(theme.success)
                } else {
                    Icon::new(IconName::X)
                        .size(ui::ICON)
                        .text_color(theme.danger)
                }))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().font_medium().child(c.name.clone()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(c.detail.clone()),
                        ),
                )
        }))
        .into_any_element()
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
