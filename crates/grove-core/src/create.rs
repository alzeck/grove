//! Planning and creating clusters: resolve PRs and branches into a plan the
//! user can adjust, then create worktrees, copy files, create databases
//! and run setup.

use crate::model::{ClusterOrigin, ClusterPhase, NoticeLevel, PrLink};
use crate::state::{CheckoutRecord, ClusterRecord, DbRecord, InstanceRecord, WorktreeSource};
use crate::{Core, CoreError, Result, now_secs};
use futures::future::join_all;
use grove_config::{Config, DEFAULT_CLUSTER, DbMode, FreshStrategy, slugify_cluster_name};
use grove_db::{DbError, Postgres, fresh_db_name};
use grove_git::WorktreeBranch;
use grove_github::{PrInput, PrState, PullRequest, RepoRef};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the user asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewClusterSource {
    /// A PR URL, `#123`, `123` or `owner/repo#123`. A bare number needs
    /// `project` unless the workspace has a single project.
    PullRequest {
        input: String,
        project: Option<String>,
    },
    /// Projects that have the branch get a worktree on it. `project` gets
    /// the branch created from `base` if it doesn't exist.
    Branch {
        branch: String,
        project: Option<String>,
        base: Option<String>,
    },
    /// Adopt a worktree created outside Grove.
    Worktree { project: String, path: PathBuf },
    /// Everything reused; adjust the plan by hand.
    Empty { name: String },
}

/// A cluster about to be created. Every field can be edited before
/// calling [`Core::create_cluster`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPlan {
    pub name: String,
    pub origin: ClusterOrigin,
    pub projects: IndexMap<String, ProjectPlan>,
    pub warnings: Vec<String>,
    /// Start processes once created.
    pub start: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPlan {
    pub checkout: PlannedCheckout,
    /// Database mode for projects with a database; `None` uses the
    /// project's default.
    pub db: Option<DbMode>,
    pub pr: Option<PrLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedCheckout {
    Reuse,
    Worktree {
        source: WorktreeSource,
    },
    Adopt {
        path: PathBuf,
        branch: Option<String>,
    },
}

impl PlannedCheckout {
    pub fn branch(&self) -> Option<&str> {
        match self {
            PlannedCheckout::Reuse => None,
            PlannedCheckout::Worktree { source } => Some(source.branch()),
            PlannedCheckout::Adopt { branch, .. } => branch.as_deref(),
        }
    }
}

impl WorktreeSource {
    pub fn branch(&self) -> &str {
        match self {
            WorktreeSource::Branch { name } | WorktreeSource::NewBranch { name, .. } => name,
            WorktreeSource::Fetch { local_branch, .. } => local_branch,
        }
    }
}

fn reuse() -> ProjectPlan {
    ProjectPlan {
        checkout: PlannedCheckout::Reuse,
        db: None,
        pr: None,
    }
}

impl Core {
    /// Resolves a request into an editable plan. Talks to GitHub and fetches
    /// branches, but changes nothing on disk except git refs.
    pub async fn plan_cluster(&self, source: NewClusterSource) -> Result<ClusterPlan> {
        let cfg = self.require_config()?;
        let mut plan = match source {
            NewClusterSource::PullRequest { input, project } => {
                self.plan_pr(&cfg, &input, project.as_deref()).await?
            }
            NewClusterSource::Branch {
                branch,
                project,
                base,
            } => {
                self.plan_branch(&cfg, &branch, project.as_deref(), base.as_deref())
                    .await?
            }
            NewClusterSource::Worktree { project, path } => {
                let proj = cfg
                    .project(&project)
                    .ok_or_else(|| CoreError::UnknownProject(project.clone()))?;
                let branch = grove_git::current_branch(&path).await?;
                let label = branch.clone().unwrap_or_else(|| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| project.clone())
                });
                let mut projects: IndexMap<_, _> =
                    cfg.projects.keys().map(|p| (p.clone(), reuse())).collect();
                projects.insert(
                    proj.name().to_string(),
                    ProjectPlan {
                        checkout: PlannedCheckout::Adopt {
                            path,
                            branch: branch.clone(),
                        },
                        db: None,
                        pr: None,
                    },
                );
                ClusterPlan {
                    name: self.unique_name(&slugify_cluster_name(&label)),
                    origin: match branch {
                        Some(branch) => ClusterOrigin::Branch { branch },
                        None => ClusterOrigin::Manual,
                    },
                    projects,
                    warnings: Vec::new(),
                    start: true,
                }
            }
            NewClusterSource::Empty { name } => ClusterPlan {
                name: self.unique_name(&slugify_cluster_name(&name)),
                origin: ClusterOrigin::Manual,
                projects: cfg.projects.keys().map(|p| (p.clone(), reuse())).collect(),
                warnings: Vec::new(),
                start: true,
            },
        };
        // Fill in database defaults for projects that get their own checkout.
        for (name, p) in plan.projects.iter_mut() {
            let db_cfg = cfg.projects[name].config.database.as_ref();
            p.db = match (&p.checkout, db_cfg) {
                (PlannedCheckout::Reuse, _) | (_, None) => None,
                (_, Some(db)) => Some(p.db.unwrap_or(db.default)),
            };
        }
        Ok(plan)
    }

    async fn plan_pr(
        &self,
        cfg: &Config,
        input: &str,
        project: Option<&str>,
    ) -> Result<ClusterPlan> {
        let repos = project_repos(cfg);
        let (project, repo, number) = match grove_github::parse_pr_input(input) {
            Some(PrInput::Url(repo, number)) => {
                let project = repos
                    .iter()
                    .find(|(_, r)| r.same_repo(&repo))
                    .map(|(p, _)| p.clone())
                    .ok_or_else(|| {
                        CoreError::Invalid(format!("no project in the workspace uses {repo}"))
                    })?;
                (project, repo, number)
            }
            Some(PrInput::Number(number)) => {
                let project = match project {
                    Some(p) => p.to_string(),
                    None if cfg.projects.len() == 1 => cfg.projects.keys()[0].clone(),
                    None => {
                        return Err(CoreError::Invalid(
                            "which project is this PR for? Paste the PR URL or pick a project"
                                .into(),
                        ));
                    }
                };
                let repo = repos
                    .iter()
                    .find(|(p, _)| *p == project)
                    .map(|(_, r)| r.clone())
                    .ok_or_else(|| {
                        CoreError::Invalid(format!("`{project}` isn't a GitHub repository"))
                    })?;
                (project, repo, number)
            }
            None => {
                return Err(CoreError::Invalid(format!(
                    "`{input}` isn't a PR URL or number"
                )));
            }
        };

        let pr = grove_github::pr_view(&repo, number).await?;
        let mut warnings = Vec::new();
        let mut projects: IndexMap<String, ProjectPlan> =
            cfg.projects.keys().map(|p| (p.clone(), reuse())).collect();

        let primary = self
            .pr_project_plan(cfg, &project, &pr, &mut warnings)
            .await?;
        projects.insert(project.clone(), primary);

        let others: Vec<RepoRef> = repos
            .iter()
            .filter(|(p, _)| *p != project)
            .map(|(_, r)| r.clone())
            .collect();
        let linked = grove_github::find_linked_prs(&pr.body, &others);
        let fetched = join_all(
            linked
                .iter()
                .map(|(repo, n)| grove_github::pr_view(repo, *n)),
        )
        .await;
        for ((repo, n), result) in linked.iter().zip(fetched) {
            let Some((linked_project, _)) = repos.iter().find(|(_, r)| r.same_repo(repo)) else {
                continue;
            };
            if projects[linked_project].checkout != PlannedCheckout::Reuse {
                warnings.push(format!(
                    "{repo}#{n} ignored: {linked_project} already uses another PR"
                ));
                continue;
            }
            match result {
                Ok(linked_pr) => {
                    let plan = self
                        .pr_project_plan(cfg, linked_project, &linked_pr, &mut warnings)
                        .await?;
                    projects.insert(linked_project.clone(), plan);
                }
                Err(e) => warnings.push(format!("couldn't load linked PR {repo}#{n}: {e}")),
            }
        }

        let base = format!("pr-{number}");
        let name = if self.name_taken(&base) {
            self.unique_name(&format!("pr-{project}-{number}"))
        } else {
            base
        };
        Ok(ClusterPlan {
            name,
            origin: ClusterOrigin::PullRequest {
                project,
                number,
                url: pr.url.clone(),
            },
            projects,
            warnings,
            start: true,
        })
    }

    async fn pr_project_plan(
        &self,
        cfg: &Config,
        project: &str,
        pr: &PullRequest,
        warnings: &mut Vec<String>,
    ) -> Result<ProjectPlan> {
        let link = PrLink {
            repo: pr.repo.clone(),
            number: pr.number,
            url: pr.url.clone(),
            title: pr.title.clone(),
            state: Some(pr.state),
        };
        if pr.state != PrState::Open {
            warnings.push(format!("{}#{} is {:?}", pr.repo, pr.number, pr.state));
        }
        let (remote_ref, local_branch) = grove_github::fetch_refspec(pr);
        let source = WorktreeSource::Fetch {
            remote_ref,
            local_branch: local_branch.clone(),
            track: !pr.is_cross_repository,
        };
        let checkout = self
            .checkout_for_branch(
                cfg,
                project,
                &local_branch,
                PlannedCheckout::Worktree { source },
                warnings,
            )
            .await?;
        Ok(ProjectPlan {
            checkout,
            db: None,
            pr: Some(link),
        })
    }

    /// Handles a branch that's already checked out somewhere: in the main
    /// clone → reuse the default cluster; in another worktree → adopt it.
    async fn checkout_for_branch(
        &self,
        cfg: &Config,
        project: &str,
        branch: &str,
        wanted: PlannedCheckout,
        warnings: &mut Vec<String>,
    ) -> Result<PlannedCheckout> {
        let main = &cfg.projects[project].main_clone;
        let Some(path) = grove_git::branch_checked_out_at(main, branch).await? else {
            return Ok(wanted);
        };
        if grove_git::same_path(&path, main) {
            warnings.push(format!(
                "{project}: `{branch}` is checked out in your main clone, so this cluster reuses the default cluster's {project}"
            ));
            return Ok(PlannedCheckout::Reuse);
        }
        if let Some(cluster) = self.cluster_using_path(&path) {
            warnings.push(format!(
                "{project}: `{branch}` is already used by cluster `{cluster}`; reusing the default instead"
            ));
            return Ok(PlannedCheckout::Reuse);
        }
        warnings.push(format!(
            "{project}: `{branch}` is checked out at {}; the cluster will use that worktree",
            path.display()
        ));
        Ok(PlannedCheckout::Adopt {
            path,
            branch: Some(branch.to_string()),
        })
    }

    async fn plan_branch(
        &self,
        cfg: &Config,
        branch: &str,
        project: Option<&str>,
        base: Option<&str>,
    ) -> Result<ClusterPlan> {
        if let Some(p) = project
            && cfg.project(p).is_none()
        {
            return Err(CoreError::UnknownProject(p.to_string()));
        }
        // Find which projects have the branch, fetching it first.
        let checks = join_all(cfg.projects.values().map(|proj| async move {
            let main = &proj.main_clone;
            if !main.join(".git").exists() {
                return (proj.name().to_string(), None);
            }
            let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
            let _ = grove_git::fetch(main, "origin", &[refspec]).await;
            let local = grove_git::branch_exists_local(main, branch)
                .await
                .unwrap_or(false);
            let remote = grove_git::branch_exists_remote(main, "origin", branch)
                .await
                .unwrap_or(false);
            (proj.name().to_string(), Some(local || remote))
        }))
        .await;

        let mut warnings = Vec::new();
        let mut projects = IndexMap::new();
        for (name, exists) in checks {
            let plan = match exists {
                None => {
                    if project == Some(name.as_str()) {
                        return Err(CoreError::MissingClone {
                            project: name.clone(),
                            path: cfg.projects[&name].main_clone.clone(),
                        });
                    }
                    reuse()
                }
                Some(true) => {
                    let wanted = PlannedCheckout::Worktree {
                        source: WorktreeSource::Branch {
                            name: branch.to_string(),
                        },
                    };
                    ProjectPlan {
                        checkout: self
                            .checkout_for_branch(cfg, &name, branch, wanted, &mut warnings)
                            .await?,
                        db: None,
                        pr: None,
                    }
                }
                Some(false) if project == Some(name.as_str()) => {
                    let base = match base {
                        Some(b) => b.to_string(),
                        None => grove_git::default_branch(&cfg.projects[&name].main_clone).await?,
                    };
                    ProjectPlan {
                        checkout: PlannedCheckout::Worktree {
                            source: WorktreeSource::NewBranch {
                                name: branch.to_string(),
                                base,
                            },
                        },
                        db: None,
                        pr: None,
                    }
                }
                Some(false) => reuse(),
            };
            projects.insert(name, plan);
        }

        if projects
            .values()
            .all(|p| p.checkout == PlannedCheckout::Reuse)
        {
            return Err(CoreError::Invalid(format!(
                "no project has a branch named `{branch}`; pick a project to create it in"
            )));
        }
        Ok(ClusterPlan {
            name: self.unique_name(&slugify_cluster_name(branch)),
            origin: ClusterOrigin::Branch {
                branch: branch.to_string(),
            },
            projects,
            warnings,
            start: true,
        })
    }

    fn name_taken(&self, name: &str) -> bool {
        name == DEFAULT_CLUSTER || self.inner.state.lock().cluster(name).is_some()
    }

    fn unique_name(&self, base: &str) -> String {
        if !self.name_taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|i| {
                let suffix = format!("-{i}");
                let keep = 30usize.saturating_sub(suffix.len()).min(base.len());
                format!("{}{suffix}", base[..keep].trim_end_matches('-'))
            })
            .find(|n| !self.name_taken(n))
            .expect("unbounded")
    }

    fn cluster_using_path(&self, path: &Path) -> Option<String> {
        let state = self.inner.state.lock();
        state.clusters.iter().find_map(|c| {
            c.instances.values().find_map(|i| match &i.checkout {
                CheckoutRecord::Worktree { path: p, .. } if grove_git::same_path(p, path) => {
                    Some(c.name.clone())
                }
                _ => None,
            })
        })
    }

    /// Creates the cluster described by `plan`: worktrees, copied files,
    /// databases and setup. Progress shows up as activities. On failure the
    /// cluster stays in an error state; [`Core::retry_cluster`] resumes it.
    pub async fn create_cluster(&self, plan: ClusterPlan) -> Result<()> {
        let cfg = self.require_config()?;
        let name = plan.name.clone();
        if !grove_config::is_valid_cluster_name(&name) {
            return Err(CoreError::Invalid(format!(
                "`{name}` isn't a valid cluster name (lowercase letters, digits and dashes, at most 30)"
            )));
        }
        if self.name_taken(&name) {
            return Err(CoreError::Invalid(format!(
                "cluster `{name}` already exists"
            )));
        }

        let mut instances = IndexMap::new();
        for (project, p) in &plan.projects {
            let proj = cfg
                .project(project)
                .ok_or_else(|| CoreError::UnknownProject(project.clone()))?;
            let checkout = match &p.checkout {
                PlannedCheckout::Reuse => CheckoutRecord::Reuse,
                PlannedCheckout::Worktree { source } => CheckoutRecord::Worktree {
                    path: self.inner.home.worktree_path(&name, project),
                    branch: Some(source.branch().to_string()),
                    adopted: false,
                    create: Some(source.clone()),
                },
                PlannedCheckout::Adopt { path, branch } => CheckoutRecord::Worktree {
                    path: path.clone(),
                    branch: branch.clone(),
                    adopted: true,
                    create: None,
                },
            };
            let reused = checkout == CheckoutRecord::Reuse;
            let db = match (&proj.config.database, reused) {
                (Some(db_cfg), false) => {
                    let mode = p.db.unwrap_or(db_cfg.default);
                    Some(match mode {
                        DbMode::Shared => DbRecord {
                            mode,
                            name: db_cfg.name.clone(),
                            created: true,
                        },
                        DbMode::Fresh => DbRecord {
                            mode,
                            name: fresh_db_name(&db_cfg.name, &name),
                            created: false,
                        },
                    })
                }
                _ => None,
            };
            instances.insert(
                project.clone(),
                InstanceRecord {
                    prepared: reused || matches!(p.checkout, PlannedCheckout::Adopt { .. }),
                    checkout,
                    db,
                    pr: p.pr.clone(),
                },
            );
        }
        if instances
            .values()
            .all(|i| i.checkout == CheckoutRecord::Reuse && i.db.is_none())
        {
            return Err(CoreError::Invalid(
                "every project is reused, so this cluster would be the default cluster".into(),
            ));
        }

        self.inner.state.lock().clusters.push(ClusterRecord {
            name: name.clone(),
            created_at: now_secs(),
            origin: plan.origin.clone(),
            instances,
            incomplete: true,
        });
        self.save_state();
        self.notify();
        self.finish_creating(&name, plan.start).await
    }

    /// Resumes creating a cluster whose creation failed.
    pub async fn retry_cluster(&self, name: &str, start: bool) -> Result<()> {
        self.require_cluster(name)?;
        self.finish_creating(name, start).await
    }

    async fn finish_creating(&self, name: &str, start: bool) -> Result<()> {
        let lock = self.op_lock(name);
        let _guard = lock.try_lock().map_err(|_| CoreError::Busy(name.into()))?;
        self.set_op(
            name,
            Some(ClusterPhase::Creating {
                step: "preparing".into(),
            }),
        );

        let result = self.prepare_cluster(name).await;
        match &result {
            Ok(()) => {
                if let Some(rec) = self.inner.state.lock().cluster_mut(name) {
                    rec.incomplete = false;
                }
                self.save_state();
                self.set_op(name, None);
                let cfg = self.require_config()?;
                self.ensure_ports(&cfg, name)?;
                self.update_routes();
            }
            Err(e) => self.set_op(
                name,
                Some(ClusterPhase::Error {
                    message: e.to_string(),
                }),
            ),
        }
        result?;
        drop(_guard);
        if start {
            self.start_cluster(name).await?;
        }
        Ok(())
    }

    pub(crate) fn set_op(&self, cluster: &str, op: Option<ClusterPhase>) {
        self.inner.rt.lock().cluster(cluster).op = op;
        self.notify();
    }

    async fn prepare_cluster(&self, name: &str) -> Result<()> {
        let projects: Vec<String> = {
            let state = self.inner.state.lock();
            let rec = state
                .cluster(name)
                .ok_or_else(|| CoreError::UnknownCluster(name.into()))?;
            rec.instances
                .iter()
                .filter(|(_, i)| !i.prepared || i.db.as_ref().is_some_and(|d| !d.created))
                .map(|(p, _)| p.clone())
                .collect()
        };
        let results = join_all(projects.iter().map(|p| self.prepare_instance(name, p))).await;
        let errors: Vec<String> = projects
            .iter()
            .zip(results)
            .filter_map(|(p, r)| r.err().map(|e| format!("{p}: {e}")))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Invalid(errors.join("; ")))
        }
    }

    fn instance_record(&self, cluster: &str, project: &str) -> Result<InstanceRecord> {
        self.inner
            .state
            .lock()
            .cluster(cluster)
            .and_then(|c| c.instances.get(project).cloned())
            .ok_or_else(|| CoreError::UnknownProject(project.into()))
    }

    fn update_instance(&self, cluster: &str, project: &str, f: impl FnOnce(&mut InstanceRecord)) {
        if let Some(inst) = self
            .inner
            .state
            .lock()
            .cluster_mut(cluster)
            .and_then(|c| c.instances.get_mut(project))
        {
            f(inst);
        }
        self.save_state();
    }

    async fn prepare_instance(&self, cluster: &str, project: &str) -> Result<()> {
        let cfg = self.require_config()?;
        let proj = cfg.projects[project].clone();
        let record = self.instance_record(cluster, project)?;
        let CheckoutRecord::Worktree { path, create, .. } = &record.checkout else {
            return Ok(());
        };
        let main = proj.main_clone.clone();
        if !main.join(".git").exists() {
            return Err(CoreError::MissingClone {
                project: project.into(),
                path: main,
            });
        }

        if !record.prepared
            && let Some(source) = create
        {
            if !path.exists() {
                self.set_op(
                    cluster,
                    Some(ClusterPhase::Creating {
                        step: format!("{project}: creating worktree"),
                    }),
                );
                self.activity(
                    cluster,
                    Some(project),
                    format!("git worktree add ({})", source.branch()),
                    create_worktree(&main, path, source),
                )
                .await?;
            }
            if !proj.config.copy_from_original.is_empty() {
                self.set_op(
                    cluster,
                    Some(ClusterPhase::Creating {
                        step: format!("{project}: copying files"),
                    }),
                );
                let report = self
                    .activity(
                        cluster,
                        Some(project),
                        "copy files from the main clone",
                        async {
                            Ok(grove_git::copy_from_original(
                                &main,
                                path,
                                &proj.config.copy_from_original,
                            )
                            .await?)
                        },
                    )
                    .await?;
                if !report.missing_patterns.is_empty() {
                    self.notice(
                        NoticeLevel::Info,
                        format!(
                            "{project}: nothing to copy for {}",
                            report.missing_patterns.join(", ")
                        ),
                    );
                }
            }
        }

        // Fresh database before setup, so setup can use it.
        let fresh_created_now = match &record.db {
            Some(db) if db.mode == DbMode::Fresh && !db.created => {
                self.set_op(
                    cluster,
                    Some(ClusterPhase::Creating {
                        step: format!("{project}: creating database {}", db.name),
                    }),
                );
                let db_cfg = proj
                    .config
                    .database
                    .clone()
                    .expect("fresh db without config");
                self.create_fresh_db(&cfg, cluster, project, &db_cfg.name, &db.name, db_cfg.fresh)
                    .await?;
                true
            }
            _ => false,
        };

        if !record.prepared {
            self.set_op(
                cluster,
                Some(ClusterPhase::Creating {
                    step: format!("{project}: setup"),
                }),
            );
            self.run_hooks(&cfg, cluster, project, &proj.config.setup)
                .await?;
            self.forget_shell_env(path);
            self.update_instance(cluster, project, |i| i.prepared = true);
        }

        if fresh_created_now {
            let db_cfg = proj.config.database.clone().expect("checked above");
            let mut hooks = Vec::new();
            hooks.extend(db_cfg.migrate.clone());
            if db_cfg.fresh == FreshStrategy::Migrate {
                hooks.extend(db_cfg.seed.clone());
            }
            self.set_op(
                cluster,
                Some(ClusterPhase::Creating {
                    step: format!("{project}: migrating"),
                }),
            );
            self.run_hooks(&cfg, cluster, project, &hooks).await?;
            self.update_instance(cluster, project, |i| {
                if let Some(db) = &mut i.db {
                    db.created = true;
                }
            });
        }
        Ok(())
    }

    async fn create_fresh_db(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        shared: &str,
        fresh: &str,
        strategy: FreshStrategy,
    ) -> Result<()> {
        let pg = Postgres::new(cfg.postgres_url());
        // A leftover from an earlier failed attempt is ours to replace.
        if pg.database_exists(fresh).await? {
            pg.drop_database(fresh).await?;
        }
        let title = match strategy {
            FreshStrategy::Dump => format!("copy database {shared} → {fresh}"),
            FreshStrategy::Template => format!("create {fresh} from template {shared}"),
            FreshStrategy::Migrate => format!("create empty database {fresh}"),
        };
        self.activity(cluster, Some(project), title, async {
            match strategy {
                FreshStrategy::Dump => match pg.dump_restore(shared, fresh).await {
                    Err(DbError::RestoreWarnings(w)) => {
                        self.notice(
                            NoticeLevel::Warning,
                            format!("{project}: pg_restore reported problems: {w}"),
                        );
                        Ok(())
                    }
                    other => Ok(other?),
                },
                FreshStrategy::Template => match pg.create_from_template(fresh, shared).await {
                    Err(DbError::TemplateInUse { connections, .. }) => {
                        let who: Vec<String> = connections.iter().map(|c| c.to_string()).collect();
                        Err(CoreError::Invalid(format!(
                            "{shared} is in use by {}. Stop the default cluster's {project} or use `fresh = \"dump\"`",
                            who.join(", ")
                        )))
                    }
                    other => Ok(other?),
                },
                FreshStrategy::Migrate => Ok(pg.create_empty(fresh).await?),
            }
        })
        .await
    }

    /// Runs setup-style commands for a project in its checkout.
    pub(crate) async fn run_hooks(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        commands: &[String],
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        let inst = self.instance(cfg, cluster, project)?;
        let dir = inst
            .dir
            .clone()
            .ok_or_else(|| CoreError::Invalid(format!("{project} is reused in {cluster}")))?;
        let env = self.build_env(cfg, cluster, project, None).await?;
        let ctx = self.template_context(cfg, cluster, project, None)?;
        for cmd in commands {
            let cmd = self.render(cmd, &ctx)?;
            self.command_activity(cluster, project, &cmd, &dir, env.clone())
                .await?;
        }
        Ok(())
    }
}

async fn create_worktree(main: &Path, path: &Path, source: &WorktreeSource) -> Result<()> {
    let branch = match source {
        WorktreeSource::Branch { name } => WorktreeBranch::Existing(name.clone()),
        WorktreeSource::NewBranch { name, base } => WorktreeBranch::New {
            name: name.clone(),
            base: base.clone(),
        },
        WorktreeSource::Fetch {
            remote_ref,
            local_branch,
            track,
        } => {
            let dest = if *track {
                format!("+{remote_ref}:refs/remotes/origin/{local_branch}")
            } else {
                format!("+{remote_ref}:refs/heads/{local_branch}")
            };
            grove_git::fetch(main, "origin", &[dest]).await?;
            WorktreeBranch::Existing(local_branch.clone())
        }
    };
    grove_git::worktree_add(main, path, &branch).await?;
    Ok(())
}

fn project_repos(cfg: &Config) -> Vec<(String, RepoRef)> {
    cfg.projects
        .iter()
        .filter_map(|(name, p)| RepoRef::from_remote_url(&p.config.repo).map(|r| (name.clone(), r)))
        .collect()
}
