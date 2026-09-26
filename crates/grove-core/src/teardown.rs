//! Removing clusters safely: report what would be lost, then stop
//! processes, remove worktrees, drop fresh databases.

use crate::model::{ClusterPhase, NoticeLevel, TeardownOptions, TeardownReport, WorktreeReport};
use crate::state::{CheckoutRecord, ClusterRecord};
use crate::{Core, CoreError, Result};
use grove_config::{DEFAULT_CLUSTER, DbMode};
use grove_db::Postgres;

impl Core {
    fn cluster_record(&self, name: &str) -> Result<ClusterRecord> {
        if name == DEFAULT_CLUSTER {
            return Err(CoreError::Invalid(
                "the default cluster can't be torn down".into(),
            ));
        }
        self.inner
            .state
            .lock()
            .cluster(name)
            .cloned()
            .ok_or_else(|| CoreError::UnknownCluster(name.into()))
    }

    /// What tearing down `name` would remove, including uncommitted changes.
    pub async fn teardown_report(&self, name: &str) -> Result<TeardownReport> {
        let record = self.cluster_record(name)?;
        let mut worktrees = Vec::new();
        let mut databases = Vec::new();
        for (project, inst) in &record.instances {
            if let CheckoutRecord::Worktree {
                path,
                branch,
                adopted,
                ..
            } = &inst.checkout
            {
                let changes = if path.exists() {
                    grove_git::status(path).await?.changes
                } else {
                    Vec::new()
                };
                worktrees.push(WorktreeReport {
                    project: project.clone(),
                    path: path.clone(),
                    branch: branch.clone(),
                    adopted: *adopted,
                    will_remove: !adopted,
                    changes,
                });
            }
            if let Some(db) = &inst.db
                && db.mode == DbMode::Fresh
            {
                databases.push(db.name.clone());
            }
        }
        Ok(TeardownReport {
            cluster: name.into(),
            worktrees,
            databases,
        })
    }

    /// Stops the cluster, removes its worktrees and fresh databases, and
    /// forgets it. Branches are never deleted. Refuses to remove dirty
    /// worktrees unless `opts.force`.
    pub async fn teardown(&self, name: &str, opts: TeardownOptions) -> Result<()> {
        let cfg = self.require_config()?;
        let record = self.cluster_record(name)?;
        let lock = self.op_lock(name);
        let _guard = lock.try_lock().map_err(|_| CoreError::Busy(name.into()))?;

        let report = self.teardown_report(name).await?;
        let dirty: Vec<String> = report
            .worktrees
            .iter()
            .filter(|w| (w.will_remove || opts.remove_adopted) && !w.changes.is_empty())
            .map(|w| format!("{} ({} changed files)", w.project, w.changes.len()))
            .collect();
        if !dirty.is_empty() && !opts.force {
            return Err(CoreError::DirtyWorktrees(dirty));
        }

        self.stop_cluster(name).await?;
        self.set_op(name, Some(ClusterPhase::TearingDown));

        let mut errors = Vec::new();
        for (project, inst) in &record.instances {
            let CheckoutRecord::Worktree { path, adopted, .. } = &inst.checkout else {
                continue;
            };
            let Some(proj) = cfg.project(project) else {
                continue;
            };
            if path.exists()
                && !proj.config.teardown.is_empty()
                && let Err(e) = self
                    .run_hooks(&cfg, name, project, &proj.config.teardown)
                    .await
            {
                self.notice(
                    NoticeLevel::Warning,
                    format!("{project}: teardown hook: {e}"),
                );
            }
            if *adopted && !opts.remove_adopted {
                continue;
            }
            let result = self
                .activity(
                    name,
                    Some(project),
                    format!("remove worktree {}", path.display()),
                    async {
                        Ok(grove_git::worktree_remove(&proj.main_clone, path, opts.force).await?)
                    },
                )
                .await;
            if let Err(e) = result {
                errors.push(format!("{project}: {e}"));
            }
            self.forget_shell_env(path);
        }

        if !errors.is_empty() {
            let message = errors.join("; ");
            self.set_op(
                name,
                Some(ClusterPhase::Error {
                    message: format!("teardown failed: {message}"),
                }),
            );
            return Err(CoreError::Invalid(message));
        }

        let pg = Postgres::new(cfg.postgres_url());
        for db in &report.databases {
            if let Err(e) = pg.drop_database(db).await {
                self.notice(
                    NoticeLevel::Warning,
                    format!("couldn't drop database {db}: {e}"),
                );
            }
        }

        let _ = std::fs::remove_dir_all(self.inner.home.cluster_logs_dir(name));
        let _ = std::fs::remove_dir(self.inner.home.worktrees_dir().join(name));
        self.inner.state.lock().remove_cluster(name);
        self.save_state();
        self.inner.rt.lock().clusters.remove(name);
        self.update_routes();
        self.notify();
        Ok(())
    }
}
