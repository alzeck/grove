//! Starting and stopping processes: dependency order, ports, readiness,
//! crash detection.

use crate::model::{ClusterPhase, ProcState};
use crate::{Core, CoreError, Result};
use futures::future::join_all;
use grove_config::{Config, DEFAULT_CLUSTER};
use grove_proc::{ManagedProcess, PidRecord, ReadyError, SpawnSpec};
use std::sync::Arc;
use std::time::Instant;

/// Outcome of waiting for a dependency.
enum DepWait {
    Satisfied,
    Failed(String),
    Cancelled,
}

impl Core {
    /// Starts every autostart process of a cluster (and whatever reused
    /// projects it needs from the default cluster), in dependency order.
    /// Returns an error summarising processes that failed.
    pub async fn start_cluster(&self, cluster: &str) -> Result<()> {
        let cfg = self.require_config()?;
        self.require_cluster(cluster)?;
        if let Some(rec) = self.inner.state.lock().cluster(cluster)
            && rec.incomplete
        {
            return Err(CoreError::Invalid(format!(
                "cluster `{cluster}` isn't fully created yet; retry creating it first"
            )));
        }
        let epoch = {
            let mut rt = self.inner.rt.lock();
            let crt = rt.cluster(cluster);
            if matches!(
                crt.op,
                Some(ClusterPhase::Creating { .. } | ClusterPhase::TearingDown)
            ) {
                return Err(CoreError::Busy(cluster.to_string()));
            }
            crt.idle = false;
            crt.last_activity = Instant::now();
            crt.epoch
        };
        self.ensure_ports(&cfg, cluster)?;
        self.update_routes();

        let mut targets = Vec::new();
        for inst in self.instances(&cfg, cluster)? {
            let eff = inst.effective_cluster().to_string();
            for (pname, p) in &cfg.projects[&inst.project].config.processes {
                if p.autostart {
                    targets.push((eff.clone(), inst.project.clone(), pname.clone()));
                }
            }
        }

        let results = join_all(targets.iter().map(|(c, p, n)| async move {
            let r = if c == cluster {
                self.start_claimed(c, p, n, Some(epoch)).await
            } else {
                // Reused from the default cluster: start only if stopped.
                self.ensure_process(c, p, n);
                Ok(())
            };
            r.map_err(|e| format!("{p}.{n}: {e}"))
        }))
        .await;
        // The idle clock starts once the cluster is up.
        self.inner.rt.lock().cluster(cluster).last_activity = Instant::now();
        self.notify();

        let failures: Vec<String> = results.into_iter().filter_map(|r| r.err()).collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Invalid(failures.join("; ")))
        }
    }

    pub async fn stop_cluster(&self, cluster: &str) -> Result<()> {
        self.require_cluster(cluster)?;
        let procs: Vec<(String, String)> = {
            let mut rt = self.inner.rt.lock();
            let crt = rt.cluster(cluster);
            crt.epoch += 1;
            crt.idle = false;
            crt.procs.keys().cloned().collect()
        };
        join_all(
            procs
                .iter()
                .map(|(p, n)| self.stop_process_inner(cluster, p, n)),
        )
        .await;
        self.notify();
        Ok(())
    }

    pub async fn restart_cluster(&self, cluster: &str) -> Result<()> {
        self.stop_cluster(cluster).await?;
        self.start_cluster(cluster).await
    }

    /// Stops a cluster because nobody used it; it wakes on the next request.
    pub(crate) async fn idle_cluster(&self, cluster: &str) {
        let _ = self.stop_cluster(cluster).await;
        self.inner.rt.lock().cluster(cluster).idle = true;
        self.notify();
    }

    pub async fn start_process(&self, cluster: &str, project: &str, process: &str) -> Result<()> {
        let cfg = self.require_config()?;
        let inst = self.instance(&cfg, cluster, project)?;
        self.require_process(&cfg, project, process)?;
        self.ensure_ports(&cfg, inst.effective_cluster())?;
        self.update_routes();
        self.inner.rt.lock().cluster(cluster).idle = false;
        self.start_claimed(inst.effective_cluster(), project, process, None)
            .await
    }

    pub async fn stop_process(&self, cluster: &str, project: &str, process: &str) -> Result<()> {
        let cfg = self.require_config()?;
        let inst = self.instance(&cfg, cluster, project)?;
        self.require_process(&cfg, project, process)?;
        self.stop_process_inner(inst.effective_cluster(), project, process)
            .await;
        self.notify();
        Ok(())
    }

    pub async fn restart_process(&self, cluster: &str, project: &str, process: &str) -> Result<()> {
        self.stop_process(cluster, project, process).await?;
        self.start_process(cluster, project, process).await
    }

    /// The live process, for attaching a terminal.
    pub fn process_handle(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
    ) -> Option<ManagedProcess> {
        let cfg = self.config()?;
        let inst = self.instance(&cfg, cluster, project).ok()?;
        let mut rt = self.inner.rt.lock();
        rt.cluster(inst.effective_cluster())
            .procs
            .get(&(project.to_string(), process.to_string()))
            .and_then(|p| p.handle.clone())
    }

    pub(crate) fn proc_state(&self, cluster: &str, project: &str, process: &str) -> ProcState {
        self.inner
            .rt
            .lock()
            .cluster(cluster)
            .proc_state(project, process)
    }

    pub(crate) async fn stop_everything(&self) {
        let names: Vec<String> = self.inner.rt.lock().clusters.keys().cloned().collect();
        join_all(names.iter().map(|c| self.stop_cluster(c))).await;
    }

    fn require_process(&self, cfg: &Config, project: &str, process: &str) -> Result<()> {
        let proj = cfg
            .project(project)
            .ok_or_else(|| CoreError::UnknownProject(project.into()))?;
        if proj.config.processes.contains_key(process) {
            Ok(())
        } else {
            Err(CoreError::UnknownProcess {
                project: project.into(),
                process: process.into(),
            })
        }
    }

    /// Marks a process as starting unless it already runs (or is about to).
    /// Returns the new generation when the caller should start it.
    fn claim(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        epoch: Option<u64>,
    ) -> Option<u64> {
        let mut rt = self.inner.rt.lock();
        let crt = rt.cluster(cluster);
        if epoch.is_some_and(|e| e != crt.epoch) {
            return None;
        }
        let prt = crt.proc_mut(project, process);
        let busy =
            prt.handle.is_some() || matches!(prt.state, ProcState::Waiting | ProcState::Starting);
        if busy {
            return None;
        }
        prt.generation += 1;
        prt.state = ProcState::Waiting;
        prt.stopping = false;
        Some(prt.generation)
    }

    /// Starts a process in the background if it isn't running. Claims it
    /// synchronously so waiters never see a stale `Stopped`.
    pub(crate) fn ensure_process(&self, cluster: &str, project: &str, process: &str) {
        if let Some(generation) = self.claim(cluster, project, process, None) {
            self.notify();
            let core = self.clone();
            let (c, p, n) = (
                cluster.to_string(),
                project.to_string(),
                process.to_string(),
            );
            tokio::spawn(async move {
                if let Err(e) = core.run_process(&c, &p, &n, generation).await {
                    tracing::warn!("{c}/{p}.{n}: {e}");
                }
            });
        }
    }

    async fn start_claimed(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        epoch: Option<u64>,
    ) -> Result<()> {
        match self.claim(cluster, project, process, epoch) {
            Some(generation) => {
                self.notify();
                self.run_process(cluster, project, process, generation)
                    .await
            }
            // Already running: wait for it like a dependency would.
            None => match self
                .wait_dependency(cluster, project, process, cluster, None)
                .await
            {
                DepWait::Satisfied | DepWait::Cancelled => Ok(()),
                DepWait::Failed(msg) => Err(CoreError::Invalid(msg)),
            },
        }
    }

    fn is_current(&self, cluster: &str, project: &str, process: &str, generation: u64) -> bool {
        let mut rt = self.inner.rt.lock();
        rt.cluster(cluster)
            .procs
            .get(&(project.to_string(), process.to_string()))
            .is_some_and(|p| p.generation == generation && !p.stopping)
    }

    fn set_state(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        generation: u64,
        state: ProcState,
    ) {
        {
            let mut rt = self.inner.rt.lock();
            let prt = rt.cluster(cluster).proc_mut(project, process);
            if prt.generation != generation {
                return;
            }
            prt.state = state;
        }
        self.notify();
    }

    async fn wait_dependency(
        &self,
        dep_cluster: &str,
        dep_project: &str,
        dep_process: &str,
        waiter_cluster: &str,
        waiter: Option<(&str, &str, u64)>,
    ) -> DepWait {
        let label = format!("{dep_project}.{dep_process}");
        self.wait_for(|core| {
            if let Some((p, n, generation)) = waiter
                && !core.is_current(waiter_cluster, p, n, generation)
            {
                return Some(DepWait::Cancelled);
            }
            match core.proc_state(dep_cluster, dep_project, dep_process) {
                ProcState::Ready | ProcState::Exited { success: true, .. } => {
                    Some(DepWait::Satisfied)
                }
                ProcState::Waiting | ProcState::Starting => None,
                ProcState::Stopped => Some(DepWait::Failed(format!("{label} was stopped"))),
                ProcState::Exited { status, .. } => {
                    Some(DepWait::Failed(format!("{label} exited ({status})")))
                }
                ProcState::Failed { message } => {
                    Some(DepWait::Failed(format!("{label} failed: {message}")))
                }
            }
        })
        .await
    }

    async fn run_process(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        generation: u64,
    ) -> Result<()> {
        let result = self
            .run_process_inner(cluster, project, process, generation)
            .await;
        if let Err(e) = &result {
            self.set_state(
                cluster,
                project,
                process,
                generation,
                ProcState::Failed {
                    message: e.to_string(),
                },
            );
        }
        result
    }

    async fn run_process_inner(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        generation: u64,
    ) -> Result<()> {
        let cfg = self.require_config()?;
        let inst = self.instance(&cfg, cluster, project)?;
        let pcfg = cfg.projects[project].config.processes[process].clone();

        for dep in &pcfg.depends_on {
            let dep_project = dep.resolve_project(project).to_string();
            let dep_inst = self.instance(&cfg, cluster, &dep_project)?;
            let dep_cluster = dep_inst.effective_cluster().to_string();
            self.ensure_process(&dep_cluster, &dep_project, &dep.process);
            match self
                .wait_dependency(
                    &dep_cluster,
                    &dep_project,
                    &dep.process,
                    cluster,
                    Some((project, process, generation)),
                )
                .await
            {
                DepWait::Satisfied => {}
                DepWait::Cancelled => return Ok(()),
                DepWait::Failed(msg) => {
                    return Err(CoreError::Invalid(format!("dependency {msg}")));
                }
            }
        }

        let dir = inst
            .dir
            .clone()
            .ok_or_else(|| CoreError::Invalid(format!("{project} is reused in {cluster}")))?;
        if !dir.exists() {
            return Err(CoreError::MissingClone {
                project: project.into(),
                path: dir,
            });
        }

        let mut port = self.port_of(cluster, project, process);
        if let Some(p) = port
            && !grove_proc::port_is_free(p)
        {
            let fixed =
                cluster == DEFAULT_CLUSTER && pcfg.port.as_ref().and_then(|c| c.default) == Some(p);
            if fixed {
                let owner = tokio::task::spawn_blocking(move || grove_proc::port_owner(p))
                    .await
                    .ok()
                    .flatten();
                return Err(CoreError::PortInUse { port: p, owner });
            }
            port = Some(self.reallocate_port(&cfg, cluster, project, process)?);
        }

        let env = self
            .build_env(&cfg, cluster, project, Some(process))
            .await?;
        let ctx = self.template_context(&cfg, cluster, project, Some(process))?;
        let command = self.render(&pcfg.run, &ctx)?;

        if !self.is_current(cluster, project, process, generation) {
            return Ok(());
        }

        let mut spec = SpawnSpec::new(command, dir.join(&pcfg.cwd));
        spec.env = env;
        spec.log_path = Some(self.inner.home.log_path(cluster, project, process));
        let handle = ManagedProcess::spawn(spec)?;

        let still_current = {
            let mut rt = self.inner.rt.lock();
            let prt = rt.cluster(cluster).proc_mut(project, process);
            if prt.generation == generation && !prt.stopping {
                prt.handle = Some(handle.clone());
                prt.port = port;
                prt.state = ProcState::Starting;
                true
            } else {
                false
            }
        };
        if !still_current {
            handle.terminate(pcfg.stop_timeout()).await;
            return Ok(());
        }
        self.record_pid(&handle);
        self.notify();
        self.monitor_exit(cluster, project, process, generation, handle.clone());

        match &pcfg.ready {
            None => {
                self.set_state(cluster, project, process, generation, ProcState::Ready);
                Ok(())
            }
            Some(check) => match grove_proc::wait_ready(check, port, &handle).await {
                Ok(()) => {
                    self.set_state(cluster, project, process, generation, ProcState::Ready);
                    Ok(())
                }
                // The exit monitor records the exit state.
                Err(ReadyError::Exited(info)) => Err(CoreError::Invalid(format!(
                    "exited before becoming ready ({info})"
                ))),
                Err(e) => Err(CoreError::Invalid(e.to_string())),
            },
        }
    }

    fn monitor_exit(
        &self,
        cluster: &str,
        project: &str,
        process: &str,
        generation: u64,
        handle: ManagedProcess,
    ) {
        let core = self.clone();
        let (c, p, n) = (
            cluster.to_string(),
            project.to_string(),
            process.to_string(),
        );
        tokio::spawn(async move {
            let exit = handle.wait().await;
            // Only act if this process is still the current one: a stop may
            // already have cleared it and recorded `Stopped`.
            let crashed = {
                let mut rt = core.inner.rt.lock();
                let prt = rt.cluster(&c).proc_mut(&p, &n);
                let current = prt.generation == generation
                    && prt.handle.as_ref().is_some_and(|h| h.pid() == handle.pid());
                if current {
                    prt.handle = None;
                    let stopping = std::mem::take(&mut prt.stopping);
                    prt.state = if stopping {
                        ProcState::Stopped
                    } else {
                        ProcState::Exited {
                            status: exit.to_string(),
                            success: exit.success(),
                        }
                    };
                    !stopping && !exit.success()
                } else {
                    false
                }
            };
            core.forget_pid(handle.pid());
            if crashed && !core.is_shutting_down() {
                core.notice(
                    crate::NoticeLevel::Warning,
                    format!("{c}: {p}.{n} exited ({exit})"),
                );
            }
            core.notify();
        });
    }

    pub(crate) async fn stop_process_inner(&self, cluster: &str, project: &str, process: &str) {
        let timeout = self
            .config()
            .and_then(|cfg| {
                cfg.project(project)
                    .and_then(|p| p.config.processes.get(process))
                    .map(|p| p.stop_timeout())
            })
            .unwrap_or(grove_config::DEFAULT_STOP_TIMEOUT);

        let (handle, generation) = {
            let mut rt = self.inner.rt.lock();
            let prt = rt.cluster(cluster).proc_mut(project, process);
            match prt.handle.clone() {
                Some(h) => {
                    prt.stopping = true;
                    (Some(h), prt.generation)
                }
                None => {
                    // Not spawned yet (waiting on deps) or already gone.
                    prt.generation += 1;
                    if !matches!(
                        prt.state,
                        ProcState::Exited { .. } | ProcState::Failed { .. }
                    ) {
                        prt.state = ProcState::Stopped;
                    }
                    (None, prt.generation)
                }
            }
        };
        self.notify();
        let Some(handle) = handle else { return };
        handle.terminate(timeout).await;
        {
            let mut rt = self.inner.rt.lock();
            let prt = rt.cluster(cluster).proc_mut(project, process);
            if prt.generation == generation {
                prt.handle = None;
                prt.state = ProcState::Stopped;
                prt.stopping = false;
            }
        }
        self.forget_pid(handle.pid());
    }

    fn record_pid(&self, handle: &ManagedProcess) {
        let Some(start_time) = handle.start_time() else {
            return;
        };
        self.inner.state.lock().running.push(PidRecord {
            pid: handle.pid(),
            start_time,
        });
        self.save_state();
    }

    fn forget_pid(&self, pid: u32) {
        self.inner.state.lock().running.retain(|r| r.pid != pid);
        self.save_state();
    }
}

#[allow(dead_code)]
fn _assert_send(core: Arc<Core>) {
    fn is_send<T: Send>(_: T) {}
    is_send(async move { core.start_cluster("x").await });
}
