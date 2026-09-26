//! One-off steps shown in a cluster's activity list: git operations, setup
//! commands, migrations… Command steps run in a PTY so the UI can show
//! their live output.

use crate::model::ActivityState;
use crate::runtime::Activity;
use crate::{Core, CoreError, Result, now_secs};
use grove_proc::{ManagedProcess, SpawnSpec};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;

impl Core {
    pub(crate) fn begin_activity(
        &self,
        cluster: &str,
        project: Option<&str>,
        title: impl Into<String>,
    ) -> u64 {
        let id = self.inner.activity_seq.fetch_add(1, Ordering::Relaxed);
        self.inner
            .rt
            .lock()
            .cluster(cluster)
            .push_activity(Activity {
                id,
                project: project.map(str::to_string),
                title: title.into(),
                state: ActivityState::Running,
                started_at: now_secs(),
                process: None,
            });
        self.notify();
        id
    }

    pub(crate) fn finish_activity<T>(&self, cluster: &str, id: u64, result: &Result<T>) {
        {
            let mut rt = self.inner.rt.lock();
            if let Some(a) = rt.cluster(cluster).activity_mut(id) {
                a.state = match result {
                    Ok(_) => ActivityState::Succeeded,
                    Err(e) => ActivityState::Failed {
                        message: e.to_string(),
                    },
                };
            }
        }
        self.notify();
    }

    /// Runs an in-process step as an activity.
    pub(crate) async fn activity<T, F>(
        &self,
        cluster: &str,
        project: Option<&str>,
        title: impl Into<String>,
        fut: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let id = self.begin_activity(cluster, project, title);
        let result = fut.await;
        self.finish_activity(cluster, id, &result);
        result
    }

    /// Runs a shell command as an activity and waits for it.
    pub(crate) async fn command_activity(
        &self,
        cluster: &str,
        project: &str,
        command: &str,
        cwd: &Path,
        env: HashMap<String, String>,
    ) -> Result<()> {
        let id = self.begin_activity(cluster, Some(project), command);
        let result = async {
            let mut spec = SpawnSpec::new(command.to_string(), cwd.to_path_buf());
            spec.env = env;
            spec.log_path = Some(self.inner.home.log_path(cluster, project, "activity"));
            let handle = ManagedProcess::spawn(spec)?;
            {
                let mut rt = self.inner.rt.lock();
                if let Some(a) = rt.cluster(cluster).activity_mut(id) {
                    a.process = Some(handle.clone());
                }
            }
            self.notify();
            let exit = handle.wait().await;
            if exit.success() {
                Ok(())
            } else {
                Err(CoreError::CommandFailed {
                    command: command.to_string(),
                    message: exit.to_string(),
                })
            }
        }
        .await;
        self.finish_activity(cluster, id, &result);
        result
    }

    /// Output of a command activity, for attaching a terminal.
    pub fn activity_handle(&self, cluster: &str, id: u64) -> Option<ManagedProcess> {
        let mut rt = self.inner.rt.lock();
        rt.cluster(cluster)
            .activity_mut(id)
            .and_then(|a| a.process.clone())
    }
}
