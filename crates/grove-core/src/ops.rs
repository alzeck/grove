//! Day-to-day actions on instances: pull, re-run setup, shells, editor,
//! browser, git/PR refresh, external worktrees.

use crate::model::{ExternalWorktree, NoticeLevel};
use crate::state::CheckoutRecord;
use crate::{Core, CoreError, Result};
use futures::future::join_all;
use grove_config::{Config, DEFAULT_CLUSTER};
use grove_proc::{ManagedProcess, SpawnSpec};
use std::path::PathBuf;

impl Core {
    fn checkout_dir(&self, cfg: &Config, cluster: &str, project: &str) -> Result<PathBuf> {
        let inst = self.instance(cfg, cluster, project)?;
        if inst.reuse {
            return Err(CoreError::Invalid(format!(
                "{project} is reused from the default cluster in `{cluster}`"
            )));
        }
        let dir = inst.dir.expect("non-reused instances have a dir");
        if !dir.exists() {
            return Err(CoreError::MissingClone {
                project: project.into(),
                path: dir,
            });
        }
        Ok(dir)
    }

    /// `git pull --ff-only`, then the project's `after_pull` hooks.
    /// Returns git's output. Processes keep running; restart them to pick
    /// up changes the dev server doesn't reload by itself.
    pub async fn pull(&self, cluster: &str, project: &str) -> Result<String> {
        let cfg = self.require_config()?;
        let dir = self.checkout_dir(&cfg, cluster, project)?;
        let output = self
            .activity(cluster, Some(project), "git pull --ff-only", async {
                // Branches created without `-u` have no upstream; track
                // origin's branch of the same name when there is one.
                let status = grove_git::status(&dir).await?;
                if let (None, Some(branch)) = (&status.upstream, &status.branch) {
                    let _ = grove_git::fetch(&dir, "origin", &[]).await;
                    if grove_git::branch_exists_remote(&dir, "origin", branch).await? {
                        grove_git::set_upstream(&dir, branch, &format!("origin/{branch}")).await?;
                    }
                }
                Ok(grove_git::pull_ff_only(&dir).await?)
            })
            .await?;
        self.forget_shell_env(&dir);
        let hooks = cfg.projects[project].config.after_pull.clone();
        self.run_hooks(&cfg, cluster, project, &hooks).await?;
        self.refresh_git_status(cluster).await;
        Ok(output)
    }

    pub async fn rerun_setup(&self, cluster: &str, project: &str) -> Result<()> {
        let cfg = self.require_config()?;
        let dir = self.checkout_dir(&cfg, cluster, project)?;
        self.forget_shell_env(&dir);
        let setup = cfg.projects[project].config.setup.clone();
        self.run_hooks(&cfg, cluster, project, &setup).await?;
        self.forget_shell_env(&dir);
        Ok(())
    }

    /// An interactive login shell in the checkout, with the same
    /// environment as the project's processes. The caller owns it; Grove
    /// kills remaining shells on shutdown.
    pub async fn open_shell(&self, cluster: &str, project: &str) -> Result<ManagedProcess> {
        let cfg = self.require_config()?;
        let inst = self.instance(&cfg, cluster, project)?;
        let eff = inst.effective_cluster().to_string();
        let dir = self.checkout_dir(&cfg, &eff, project)?;
        let env = self.build_env(&cfg, &eff, project, None).await?;
        let mut spec = SpawnSpec::new("exec \"$SHELL\" -l".to_string(), dir);
        spec.env = env;
        let shell = ManagedProcess::spawn(spec)?;
        self.inner.rt.lock().shells.push(shell.clone());
        Ok(shell)
    }

    /// Opens the checkout in the configured editor (or the first of Zed,
    /// Cursor, VS Code found; otherwise Finder).
    pub async fn open_in_editor(&self, cluster: &str, project: &str) -> Result<()> {
        let cfg = self.require_config()?;
        let inst = self.instance(&cfg, cluster, project)?;
        let dir = self.checkout_dir(&cfg, inst.effective_cluster(), project)?;
        let env = self.shell_env(&dir).await;
        let template = match cfg.editor() {
            Some(t) => t.to_string(),
            None => detect_editor(&env),
        };
        let ctx = serde_json::json!({ "path": shell_quote(&dir.display().to_string()) });
        let command = self.render(&template, &ctx)?;
        spawn_detached(&command, &dir, &env)
    }

    pub fn open_url(&self, url: &str) -> Result<()> {
        let env: std::collections::HashMap<String, String> = std::env::vars().collect();
        spawn_detached(
            &format!("open {}", shell_quote(url)),
            &std::env::temp_dir(),
            &env,
        )
    }

    /// Reads `git status` for every checkout in the cluster.
    pub async fn refresh_git_status(&self, cluster: &str) {
        let Some(cfg) = self.config() else { return };
        let Ok(instances) = self.instances(&cfg, cluster) else {
            return;
        };
        let dirs: Vec<(String, PathBuf)> = instances
            .into_iter()
            .filter(|i| !i.reuse)
            .filter_map(|i| i.dir.map(|d| (i.project, d)))
            .filter(|(_, d)| d.exists())
            .collect();
        let results = join_all(dirs.iter().map(|(_, d)| grove_git::status(d))).await;
        {
            let mut rt = self.inner.rt.lock();
            let crt = rt.cluster(cluster);
            for ((project, _), result) in dirs.iter().zip(results) {
                match result {
                    Ok(s) => {
                        crt.git.insert(project.clone(), s);
                    }
                    Err(e) => tracing::debug!("git status {project}: {e}"),
                }
            }
        }
        self.notify();
    }

    /// Refreshes title and state of the cluster's PRs from GitHub.
    pub async fn refresh_prs(&self, cluster: &str) -> Result<()> {
        let prs: Vec<(String, crate::PrLink)> = {
            let state = self.inner.state.lock();
            let Some(rec) = state.cluster(cluster) else {
                return Err(CoreError::UnknownCluster(cluster.into()));
            };
            rec.instances
                .iter()
                .filter_map(|(p, i)| i.pr.clone().map(|pr| (p.clone(), pr)))
                .collect()
        };
        let results = join_all(
            prs.iter()
                .map(|(_, pr)| grove_github::pr_view(&pr.repo, pr.number)),
        )
        .await;
        {
            let mut state = self.inner.state.lock();
            if let Some(rec) = state.cluster_mut(cluster) {
                for ((project, _), result) in prs.iter().zip(results) {
                    let Ok(pr) = result else { continue };
                    if let Some(link) = rec.instances.get_mut(project).and_then(|i| i.pr.as_mut()) {
                        link.title = pr.title;
                        link.state = Some(pr.state);
                    }
                }
            }
        }
        self.save_state();
        self.notify();
        Ok(())
    }

    /// Worktrees of main clones that no cluster uses (made by other tools,
    /// or left behind).
    pub async fn external_worktrees(&self) -> Result<Vec<ExternalWorktree>> {
        let cfg = self.require_config()?;
        let used: Vec<PathBuf> = {
            let state = self.inner.state.lock();
            state
                .clusters
                .iter()
                .flat_map(|c| c.instances.values())
                .filter_map(|i| match &i.checkout {
                    CheckoutRecord::Worktree { path, .. } => Some(path.clone()),
                    CheckoutRecord::Reuse => None,
                })
                .collect()
        };
        let mut out = Vec::new();
        for proj in cfg.projects.values() {
            if !proj.main_clone.join(".git").exists() {
                continue;
            }
            match grove_git::worktree_list(&proj.main_clone).await {
                Ok(list) => {
                    for wt in list.into_iter().filter(|w| !w.is_main && !w.bare) {
                        if used.iter().any(|u| grove_git::same_path(u, &wt.path)) {
                            continue;
                        }
                        out.push(ExternalWorktree {
                            project: proj.name().to_string(),
                            path: wt.path,
                            branch: wt.branch,
                        });
                    }
                }
                Err(e) => self.notice(
                    NoticeLevel::Warning,
                    format!("{}: can't list worktrees: {e}", proj.name()),
                ),
            }
        }
        Ok(out)
    }

    /// Whether `cluster` is the default cluster.
    pub fn is_default(cluster: &str) -> bool {
        cluster == DEFAULT_CLUSTER
    }
}

fn detect_editor(env: &std::collections::HashMap<String, String>) -> String {
    let path = env.get("PATH").cloned().unwrap_or_default();
    for (bin, template) in [
        ("zed", "zed {{ path }}"),
        ("cursor", "cursor {{ path }}"),
        ("code", "code {{ path }}"),
    ] {
        if which::which_in(bin, Some(&path), "/").is_ok() {
            return template.into();
        }
    }
    "open {{ path }}".into()
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn spawn_detached(
    command: &str,
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reap it so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
