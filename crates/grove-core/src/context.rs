//! Resolving where each project runs, ports, hosts, URLs, template
//! context, process environment, proxy routes and the snapshot.

use crate::model::*;
use crate::state::{CheckoutRecord, DbRecord, port_key};
use crate::{Core, CoreError, Result};
use grove_config::{Config, DEFAULT_CLUSTER, DbMode};
use grove_db::Postgres;
use grove_proxy::Route;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

/// Where one project runs within one cluster.
#[derive(Debug, Clone)]
pub(crate) struct Instance {
    pub cluster: String,
    pub project: String,
    /// Runs in the default cluster instead.
    pub reuse: bool,
    pub dir: Option<PathBuf>,
    pub branch: Option<String>,
    pub adopted: bool,
    pub db: Option<DbRecord>,
}

impl Instance {
    /// The cluster whose processes serve this project.
    pub fn effective_cluster(&self) -> &str {
        if self.reuse {
            DEFAULT_CLUSTER
        } else {
            &self.cluster
        }
    }
}

impl Core {
    pub(crate) fn cluster_names(&self) -> Vec<String> {
        let mut names = vec![DEFAULT_CLUSTER.to_string()];
        names.extend(
            self.inner
                .state
                .lock()
                .clusters
                .iter()
                .map(|c| c.name.clone()),
        );
        names
    }

    pub(crate) fn require_cluster(&self, name: &str) -> Result<()> {
        if name == DEFAULT_CLUSTER || self.inner.state.lock().cluster(name).is_some() {
            Ok(())
        } else {
            Err(CoreError::UnknownCluster(name.to_string()))
        }
    }

    pub(crate) fn instance(&self, cfg: &Config, cluster: &str, project: &str) -> Result<Instance> {
        let proj = cfg
            .project(project)
            .ok_or_else(|| CoreError::UnknownProject(project.to_string()))?;
        let shared_db = proj.config.database.as_ref().map(|d| DbRecord {
            mode: DbMode::Shared,
            name: d.name.clone(),
            created: true,
        });

        if cluster == DEFAULT_CLUSTER {
            return Ok(Instance {
                cluster: cluster.into(),
                project: project.into(),
                reuse: false,
                dir: Some(proj.main_clone.clone()),
                branch: None,
                adopted: false,
                db: shared_db,
            });
        }

        let state = self.inner.state.lock();
        let record = state
            .cluster(cluster)
            .ok_or_else(|| CoreError::UnknownCluster(cluster.to_string()))?;
        // Projects added to the workspace after the cluster was created
        // behave as reused.
        let Some(inst) = record.instances.get(project) else {
            return Ok(reuse_instance(cluster, project, shared_db));
        };
        Ok(match &inst.checkout {
            CheckoutRecord::Reuse => reuse_instance(cluster, project, shared_db),
            CheckoutRecord::Worktree {
                path,
                branch,
                adopted,
                ..
            } => Instance {
                cluster: cluster.into(),
                project: project.into(),
                reuse: false,
                dir: Some(path.clone()),
                branch: branch.clone(),
                adopted: *adopted,
                db: inst.db.clone().or(shared_db),
            },
        })
    }

    pub(crate) fn instances(&self, cfg: &Config, cluster: &str) -> Result<Vec<Instance>> {
        cfg.projects
            .keys()
            .map(|p| self.instance(cfg, cluster, p))
            .collect()
    }

    /// Allocates ports for every process with `port` in every non-reused
    /// instance of `cluster`. Existing allocations are kept.
    pub(crate) fn ensure_ports(&self, cfg: &Config, cluster: &str) -> Result<()> {
        let instances = self.instances(cfg, cluster)?;
        let (lo, hi) = cfg.port_range();
        let mut state = self.inner.state.lock();
        let mut changed = false;
        for inst in instances.iter().filter(|i| !i.reuse) {
            let proj = &cfg.projects[&inst.project];
            for (pname, p) in &proj.config.processes {
                let Some(port_cfg) = &p.port else { continue };
                let key = port_key(cluster, &inst.project, pname);
                if state.ports.contains_key(&key) {
                    continue;
                }
                let port = match (cluster == DEFAULT_CLUSTER, port_cfg.default) {
                    (true, Some(fixed)) => fixed,
                    _ => pick_port(&state.ports, lo, hi)?,
                };
                state.ports.insert(key, port);
                changed = true;
            }
        }
        drop(state);
        if changed {
            self.save_state();
        }
        Ok(())
    }

    pub(crate) fn ensure_default_ports(&self) {
        if let Some(cfg) = self.config()
            && let Err(e) = self.ensure_ports(&cfg, DEFAULT_CLUSTER)
        {
            self.notice(NoticeLevel::Warning, format!("port allocation: {e}"));
        }
    }

    pub(crate) fn port_of(&self, cluster: &str, project: &str, process: &str) -> Option<u16> {
        self.inner
            .state
            .lock()
            .ports
            .get(&port_key(cluster, project, process))
            .copied()
    }

    /// Picks a new port for a process whose port was taken by something else.
    pub(crate) fn reallocate_port(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        process: &str,
    ) -> Result<u16> {
        let (lo, hi) = cfg.port_range();
        let mut state = self.inner.state.lock();
        let port = pick_port(&state.ports, lo, hi)?;
        state
            .ports
            .insert(port_key(cluster, project, process), port);
        drop(state);
        self.save_state();
        self.update_routes();
        Ok(port)
    }

    pub(crate) fn host_of(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        process: &str,
    ) -> Option<String> {
        let p = cfg.project(project)?.config.processes.get(process)?;
        let domain = p.domain.as_ref()?;
        cfg.domain_templates()
            .render_host(
                &self.inner.templates,
                domain,
                project,
                process,
                cluster,
                cluster == DEFAULT_CLUSTER,
            )
            .ok()
    }

    pub(crate) fn url_for_host(&self, host: &str) -> String {
        match *self.inner.https_port.lock() {
            443 => format!("https://{host}"),
            port => format!("https://{host}:{port}"),
        }
    }

    /// Public URL of a process, through the proxy.
    pub fn url(&self, cluster: &str, project: &str, process: &str) -> Option<String> {
        let cfg = self.config()?;
        let inst = self.instance(&cfg, cluster, project).ok()?;
        self.host_of(&cfg, inst.effective_cluster(), project, process)
            .map(|h| self.url_for_host(&h))
    }

    pub(crate) fn database_url(&self, cfg: &Config, db: &DbRecord) -> String {
        Postgres::new(cfg.postgres_url()).database_url(&db.name)
    }

    /// Everything templates can reference, for one project (and optionally
    /// one process) in one cluster.
    pub(crate) fn template_context(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        process: Option<&str>,
    ) -> Result<Value> {
        let mut projects = Map::new();
        for inst in self.instances(cfg, cluster)? {
            let serving = if inst.reuse {
                self.instance(cfg, DEFAULT_CLUSTER, &inst.project)?
            } else {
                inst.clone()
            };
            let eff = serving.cluster.clone();
            let mut entry = Map::new();
            entry.insert(
                "dir".into(),
                json!(serving.dir.as_ref().map(|d| d.display().to_string())),
            );
            entry.insert("branch".into(), json!(serving.branch));
            if let Some(db) = &serving.db {
                entry.insert(
                    "db".into(),
                    json!({ "name": db.name, "url": self.database_url(cfg, db) }),
                );
            }
            for pname in cfg.projects[&inst.project].config.processes.keys() {
                let port = self.port_of(&eff, &inst.project, pname);
                let host = self.host_of(cfg, &eff, &inst.project, pname);
                entry.insert(
                    pname.clone(),
                    json!({
                        "port": port,
                        "host": host,
                        "url": host.as_ref().map(|h| self.url_for_host(h)),
                        "local_url": port.map(|p| format!("http://127.0.0.1:{p}")),
                    }),
                );
            }
            projects.insert(inst.project.clone(), Value::Object(entry));
        }

        let this = self.instance(cfg, cluster, project)?;
        let mut self_entry = Map::new();
        self_entry.insert(
            "dir".into(),
            json!(this.dir.as_ref().map(|d| d.display().to_string())),
        );
        if let Some(process) = process {
            let port = self.port_of(cluster, project, process);
            let host = self.host_of(cfg, cluster, project, process);
            self_entry.insert("port".into(), json!(port));
            self_entry.insert("host".into(), json!(host));
            self_entry.insert(
                "url".into(),
                json!(host.as_ref().map(|h| self.url_for_host(h))),
            );
        }

        Ok(json!({
            "cluster": cluster,
            "project": project,
            "process": process.unwrap_or(""),
            "self": self_entry,
            "projects": projects,
            "postgres": { "url": cfg.postgres_url() },
            "grove": {
                "home": self.inner.home.root().display().to_string(),
                "ca_cert": self.ca_cert_path().map(|p| p.display().to_string()),
            },
        }))
    }

    pub(crate) fn render(&self, template: &str, ctx: &Value) -> Result<String> {
        Ok(self.inner.templates.render(template, ctx)?)
    }

    /// Login-shell environment for a directory, captured once and cached.
    pub(crate) async fn shell_env(&self, dir: &std::path::Path) -> Arc<HashMap<String, String>> {
        if let Some(env) = self.inner.env_cache.lock().get(dir) {
            return env.clone();
        }
        let env = match grove_proc::capture_shell_env(dir).await {
            Ok(env) => env,
            Err(e) => {
                self.notice(
                    NoticeLevel::Warning,
                    format!(
                        "couldn't load your shell environment in {}: {e}; using Grove's own",
                        dir.display()
                    ),
                );
                std::env::vars().collect()
            }
        };
        let env = Arc::new(env);
        self.inner
            .env_cache
            .lock()
            .insert(dir.to_path_buf(), env.clone());
        env
    }

    pub(crate) fn forget_shell_env(&self, dir: &std::path::Path) {
        self.inner.env_cache.lock().remove(dir);
    }

    /// Full environment for a process (or for a setup/hook command when
    /// `process` is None).
    pub(crate) async fn build_env(
        &self,
        cfg: &Config,
        cluster: &str,
        project: &str,
        process: Option<&str>,
    ) -> Result<HashMap<String, String>> {
        let inst = self.instance(cfg, cluster, project)?;
        let dir = inst
            .dir
            .clone()
            .ok_or_else(|| CoreError::Invalid(format!("{project} is reused in {cluster}")))?;
        let proj = &cfg.projects[project];
        let ctx = self.template_context(cfg, cluster, project, process)?;

        let mut env: HashMap<String, String> = (*self.shell_env(&dir).await).clone();
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.insert("GROVE_CLUSTER".into(), cluster.into());
        env.insert("GROVE_PROJECT".into(), project.into());
        if let Some(p) = process {
            env.insert("GROVE_PROCESS".into(), p.into());
        }
        if let Some(ca) = self.ca_cert_path() {
            env.insert("NODE_EXTRA_CA_CERTS".into(), ca.display().to_string());
        }
        if let (Some(db_cfg), Some(db)) = (&proj.config.database, &inst.db) {
            env.insert(db_cfg.env.clone(), self.database_url(cfg, db));
        }
        if let Some(pname) = process {
            let p = proj
                .config
                .processes
                .get(pname)
                .ok_or_else(|| CoreError::UnknownProcess {
                    project: project.into(),
                    process: pname.into(),
                })?;
            if let (Some(port_cfg), Some(port)) = (&p.port, self.port_of(cluster, project, pname)) {
                env.insert(port_cfg.env.clone(), port.to_string());
            }
        }
        for (k, v) in &proj.config.env {
            env.insert(k.clone(), self.render(v, &ctx)?);
        }
        if let Some(pname) = process {
            for (k, v) in &proj.config.processes[pname].env {
                env.insert(k.clone(), self.render(v, &ctx)?);
            }
        }
        Ok(env)
    }

    pub(crate) fn compute_routes(&self, cfg: &Config) -> Vec<Route> {
        let mut routes = Vec::new();
        for cluster in self.cluster_names() {
            let Ok(instances) = self.instances(cfg, &cluster) else {
                continue;
            };
            for inst in instances.iter().filter(|i| !i.reuse) {
                for (pname, p) in &cfg.projects[&inst.project].config.processes {
                    let Some(domain) = &p.domain else { continue };
                    let (Some(host), Some(port)) = (
                        self.host_of(cfg, &cluster, &inst.project, pname),
                        self.port_of(&cluster, &inst.project, pname),
                    ) else {
                        continue;
                    };
                    routes.push(Route {
                        host,
                        wildcard: domain.wildcard,
                        cluster: cluster.clone(),
                        label: format!("{}.{pname}", inst.project),
                        port,
                    });
                }
            }
        }
        routes
    }

    pub(crate) fn update_routes(&self) {
        let Some(cfg) = self.config() else { return };
        let routes = self.compute_routes(&cfg);
        if let Some(proxy) = &*self.inner.proxy.lock() {
            proxy.set_routes(routes);
        }
    }

    pub(crate) fn cluster_has_routes(&self, cluster: &str) -> bool {
        self.config()
            .map(|cfg| {
                self.compute_routes(&cfg)
                    .iter()
                    .any(|r| r.cluster == cluster)
            })
            .unwrap_or(false)
    }

    pub fn phase(&self, cluster: &str) -> ClusterPhase {
        let mut rt = self.inner.rt.lock();
        rt.cluster(cluster).derive_phase()
    }

    pub fn snapshot(&self) -> Snapshot {
        let config = match &*self.inner.config.lock() {
            crate::ConfigSlot::Loaded(cfg) => ConfigStatus::Loaded {
                workspace: cfg.workspace.config.name.clone(),
                workspace_dir: cfg.workspace.dir.clone(),
                warnings: cfg.warnings.iter().map(Into::into).collect(),
            },
            crate::ConfigSlot::Missing(status) => status.clone(),
        };
        let proxy = self.inner.proxy_status.lock().clone();
        let Some(cfg) = self.config() else {
            return Snapshot {
                config,
                proxy,
                clusters: Vec::new(),
            };
        };

        // Read ports before taking the runtime lock (lock order: state, then rt).
        let ports = self.inner.state.lock().ports.clone();
        let port = |c: &str, p: &str, n: &str| ports.get(&port_key(c, p, n)).copied();
        let mut clusters = Vec::new();
        for name in self.cluster_names() {
            let (origin, created_at, prs) = if name == DEFAULT_CLUSTER {
                (ClusterOrigin::Default, 0, HashMap::new())
            } else {
                let state = self.inner.state.lock();
                let Some(rec) = state.cluster(&name) else {
                    continue;
                };
                let prs: HashMap<String, PrLink> = rec
                    .instances
                    .iter()
                    .filter_map(|(p, i)| i.pr.clone().map(|pr| (p.clone(), pr)))
                    .collect();
                (rec.origin.clone(), rec.created_at, prs)
            };
            let Ok(instances) = self.instances(&cfg, &name) else {
                continue;
            };

            let rt = &mut *self.inner.rt.lock();
            let crt = rt.cluster(&name);
            let phase = crt.derive_phase();
            let activities = crt.activities.iter().map(|a| a.view()).collect();

            let instance_views = instances
                .iter()
                .map(|inst| {
                    let proj = &cfg.projects[&inst.project];
                    let checkout = if inst.reuse {
                        CheckoutView::Reuse
                    } else if name == DEFAULT_CLUSTER {
                        let path = inst.dir.clone().unwrap_or_default();
                        CheckoutView::MainClone {
                            exists: path.join(".git").exists(),
                            path,
                        }
                    } else {
                        CheckoutView::Worktree {
                            path: inst.dir.clone().unwrap_or_default(),
                            branch: inst.branch.clone(),
                            adopted: inst.adopted,
                        }
                    };
                    let processes = if inst.reuse {
                        Vec::new()
                    } else {
                        proj.config
                            .processes
                            .iter()
                            .map(|(pname, p)| {
                                let prt = crt.procs.get(&(inst.project.clone(), pname.clone()));
                                ProcessView {
                                    name: pname.clone(),
                                    state: prt
                                        .map(|r| r.state.clone())
                                        .unwrap_or(ProcState::Stopped),
                                    port: port(&name, &inst.project, pname),
                                    url: self
                                        .host_of(&cfg, &name, &inst.project, pname)
                                        .map(|h| self.url_for_host(&h)),
                                    pid: prt.and_then(|r| r.handle.as_ref().map(|h| h.pid())),
                                    autostart: p.autostart,
                                }
                            })
                            .collect()
                    };
                    InstanceView {
                        project: inst.project.clone(),
                        checkout,
                        database: if inst.reuse {
                            None
                        } else {
                            inst.db.as_ref().map(|d| DatabaseView {
                                mode: d.mode,
                                name: d.name.clone(),
                            })
                        },
                        pr: prs.get(&inst.project).cloned(),
                        git: crt.git.get(&inst.project).cloned(),
                        processes,
                    }
                })
                .collect();

            clusters.push(ClusterView {
                is_default: name == DEFAULT_CLUSTER,
                name,
                origin,
                phase,
                created_at,
                instances: instance_views,
                activities,
            });
        }
        Snapshot {
            config,
            proxy,
            clusters,
        }
    }
}

fn reuse_instance(cluster: &str, project: &str, shared_db: Option<DbRecord>) -> Instance {
    Instance {
        cluster: cluster.into(),
        project: project.into(),
        reuse: true,
        dir: None,
        branch: None,
        adopted: false,
        db: shared_db,
    }
}

fn pick_port(allocated: &std::collections::BTreeMap<String, u16>, lo: u16, hi: u16) -> Result<u16> {
    let used: BTreeSet<u16> = allocated.values().copied().collect();
    (lo..=hi)
        .find(|p| !used.contains(p) && grove_proc::port_is_free(*p))
        .ok_or(CoreError::NoFreePort(lo, hi))
}
