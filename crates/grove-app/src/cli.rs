//! CLI commands. Everything except `validate` and `doctor` goes through the
//! running app over IPC (starting it if needed).

use crate::{Command, NewArgs};
use anyhow::{Context, bail};
use grove_config::{Config, GroveHome, load_workspace};
use grove_core::{
    CheckoutView, ClusterOrigin, ClusterPlan, ClusterView, ConfigStatus, DbMode, DoctorCheck,
    ExternalWorktree, NewClusterSource, PlannedCheckout, Snapshot, TeardownOptions, TeardownReport,
};
use grove_ipc::Request;
use serde::de::DeserializeOwned;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

pub fn run(command: Command) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(run_async(command)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("grove: {e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Client {
    home: GroveHome,
}

impl Client {
    async fn call<T: DeserializeOwned>(&self, request: Request) -> anyhow::Result<T> {
        crate::launch::ensure_running(&self.home).await?;
        let value = grove_ipc::call(&self.home.socket_path(), &request, |text| {
            print!("{text}");
            let _ = std::io::stdout().flush();
        })
        .await?;
        Ok(serde_json::from_value(value)?)
    }

    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        self.call(Request::Snapshot).await
    }

    async fn cluster(&self, name: &str) -> anyhow::Result<ClusterView> {
        self.snapshot()
            .await?
            .clusters
            .into_iter()
            .find(|c| c.name == name)
            .with_context(|| format!("no cluster named `{name}`"))
    }
}

async fn run_async(command: Command) -> anyhow::Result<ExitCode> {
    let client = Client {
        home: GroveHome::from_env(),
    };
    match command {
        Command::Headless { .. } => unreachable!("handled in main"),
        Command::Ls { json } => {
            let snap = client.snapshot().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap.clusters)?);
            } else {
                print_config_problems(&snap.config);
                print_list(&snap);
            }
        }
        Command::Status { cluster, json } => {
            let snap = client.snapshot().await?;
            let clusters: Vec<&ClusterView> = match &cluster {
                Some(name) => vec![
                    snap.cluster(name)
                        .with_context(|| format!("no cluster named `{name}`"))?,
                ],
                None => snap.clusters.iter().collect(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&clusters)?);
            } else {
                print_config_problems(&snap.config);
                for c in clusters {
                    print_cluster(c);
                    println!();
                }
            }
        }
        Command::New(args) => return new_cluster(&client, args).await,
        Command::Up { cluster, target } => {
            let target = target.map(|t| parse_process(&t)).transpose()?;
            let () = client
                .call(Request::Start {
                    cluster: cluster.clone(),
                    target,
                })
                .await?;
            print_cluster(&client.cluster(&cluster).await?);
        }
        Command::Down { cluster, target } => {
            let target = target.map(|t| parse_process(&t)).transpose()?;
            let () = client.call(Request::Stop { cluster, target }).await?;
        }
        Command::Restart { cluster, target } => {
            let target = target.map(|t| parse_process(&t)).transpose()?;
            let () = client
                .call(Request::Restart {
                    cluster: cluster.clone(),
                    target,
                })
                .await?;
            print_cluster(&client.cluster(&cluster).await?);
        }
        Command::Rm {
            cluster,
            yes,
            force,
            remove_adopted,
        } => return remove_cluster(&client, cluster, yes, force, remove_adopted).await,
        Command::Retry { cluster, no_start } => {
            let () = client
                .call(Request::Retry {
                    cluster: cluster.clone(),
                    start: !no_start,
                })
                .await?;
            print_cluster(&client.cluster(&cluster).await?);
        }
        Command::Pull { cluster, project } => {
            let output: String = client.call(Request::Pull { cluster, project }).await?;
            print!("{output}");
        }
        Command::Logs {
            cluster,
            target,
            follow,
        } => {
            let (project, process) = parse_process(&target)?;
            let () = client
                .call(Request::Logs {
                    cluster,
                    project,
                    process,
                    follow,
                })
                .await?;
        }
        Command::Open { cluster, target } => {
            let c = client.cluster(&cluster).await?;
            let urls = c.urls();
            let url = match target {
                None => urls.first().map(|(_, u)| u.clone()),
                Some(t) => {
                    let wanted = t.replace('/', ".");
                    urls.iter()
                        .find(|(label, _)| {
                            *label == wanted || label.starts_with(&format!("{wanted}."))
                        })
                        .map(|(_, u)| u.clone())
                }
            }
            .with_context(|| format!("no URL found in `{cluster}`"))?;
            println!("{url}");
            let () = client.call(Request::OpenUrl { url }).await?;
        }
        Command::Edit { cluster, project } => {
            let () = client.call(Request::Edit { cluster, project }).await?;
        }
        Command::Worktrees => {
            let list: Vec<ExternalWorktree> = client.call(Request::ExternalWorktrees).await?;
            if list.is_empty() {
                println!("No worktrees outside Grove's clusters.");
            }
            for w in list {
                println!(
                    "{:<12} {:<30} {}",
                    w.project,
                    w.branch.unwrap_or_else(|| "(detached)".into()),
                    w.path.display()
                );
            }
        }
        Command::Validate { path } => return validate(&client.home, path),
        Command::Doctor => {
            let checks: Vec<DoctorCheck> =
                if grove_ipc::is_running(&client.home.socket_path()).await {
                    client.call(Request::Doctor).await?
                } else {
                    local_doctor(&client.home).await?
                };
            let mut ok = true;
            for c in &checks {
                ok &= c.ok;
                println!(
                    "{} {:<20} {}",
                    if c.ok { "✓" } else { "✗" },
                    c.name,
                    c.detail
                );
            }
            return Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Command::Reload => {
            let status: ConfigStatus = client.call(Request::Reload).await?;
            match &status {
                ConfigStatus::Loaded { workspace, .. } => {
                    println!("Loaded workspace `{workspace}`.")
                }
                _ => print_config_problems(&status),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_process(target: &str) -> anyhow::Result<(String, String)> {
    match target.split_once(['/', '.']) {
        Some((p, n)) if !p.is_empty() && !n.is_empty() => Ok((p.to_string(), n.to_string())),
        _ => bail!("expected `project/process`, got `{target}`"),
    }
}

async fn new_cluster(client: &Client, args: NewArgs) -> anyhow::Result<ExitCode> {
    let source = match (args.pr, args.branch) {
        (Some(input), _) => NewClusterSource::PullRequest {
            input,
            project: args.project.clone(),
        },
        (None, Some(branch)) => NewClusterSource::Branch {
            branch,
            project: args.project.clone(),
            base: args.base.clone(),
        },
        (None, None) => bail!("pass --pr or --branch"),
    };
    let mut plan: ClusterPlan = client.call(Request::Plan { source }).await?;
    if let Some(name) = args.name {
        plan.name = name;
    }
    for p in &args.reuse {
        let entry = plan
            .projects
            .get_mut(p)
            .with_context(|| format!("no project `{p}`"))?;
        entry.checkout = PlannedCheckout::Reuse;
        entry.db = None;
    }
    for (list, mode) in [
        (&args.fresh_db, DbMode::Fresh),
        (&args.shared_db, DbMode::Shared),
    ] {
        for p in list {
            let entry = plan
                .projects
                .get_mut(p)
                .with_context(|| format!("no project `{p}`"))?;
            if entry.checkout == PlannedCheckout::Reuse {
                bail!("`{p}` is reused, so it uses the default cluster's database");
            }
            if entry.db.is_none() {
                bail!("`{p}` has no database");
            }
            entry.db = Some(mode);
        }
    }
    plan.start = !args.no_start;

    print_plan(&plan);
    if !args.yes && std::io::stdin().is_terminal() && !confirm("Create it?")? {
        return Ok(ExitCode::FAILURE);
    }
    let name = plan.name.clone();
    println!("Creating {name}…");
    let _: String = client.call(Request::Create { plan }).await?;
    print_cluster(&client.cluster(&name).await?);
    Ok(ExitCode::SUCCESS)
}

async fn remove_cluster(
    client: &Client,
    cluster: String,
    yes: bool,
    force: bool,
    remove_adopted: bool,
) -> anyhow::Result<ExitCode> {
    let report: TeardownReport = client
        .call(Request::TeardownReport {
            cluster: cluster.clone(),
        })
        .await?;
    println!("Tearing down {cluster} will:");
    println!("  • stop its processes");
    let mut dirty = false;
    for w in &report.worktrees {
        let removing = w.will_remove || remove_adopted;
        let verb = if removing { "remove" } else { "keep (adopted)" };
        println!(
            "  • {verb} worktree {} ({}) at {}",
            w.project,
            w.branch.as_deref().unwrap_or("detached"),
            w.path.display()
        );
        if removing && !w.changes.is_empty() {
            dirty = true;
            println!("    ⚠ {} uncommitted change(s):", w.changes.len());
            for c in w.changes.iter().take(10) {
                println!("      {:?} {}", c.kind, c.path);
            }
            if w.changes.len() > 10 {
                println!("      … and {} more", w.changes.len() - 10);
            }
        }
    }
    for db in &report.databases {
        println!("  • drop database {db}");
    }
    println!("  Branches are kept.");

    if dirty && !force {
        eprintln!(
            "\nSome worktrees have uncommitted changes. Commit or stash them, or pass --force."
        );
        return Ok(ExitCode::FAILURE);
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to confirm when not running interactively");
        }
        if !confirm("Continue?")? {
            return Ok(ExitCode::FAILURE);
        }
    }
    let () = client
        .call(Request::Teardown {
            cluster: cluster.clone(),
            options: TeardownOptions {
                force,
                remove_adopted,
            },
        })
        .await?;
    println!("Removed {cluster}.");
    Ok(ExitCode::SUCCESS)
}

fn validate(home: &GroveHome, path: Option<std::path::PathBuf>) -> anyhow::Result<ExitCode> {
    let (dir, overrides) = match path {
        Some(p) => (p, Default::default()),
        None => {
            let user = Config::load_user(home)?;
            (Config::workspace_dir(home, &user)?, user.overrides)
        }
    };
    match load_workspace(&dir, &overrides) {
        Ok(ws) => {
            println!(
                "✓ {} is valid: workspace `{}`, {} project(s)",
                dir.display(),
                ws.config.name,
                ws.projects.len()
            );
            for w in &ws.warnings {
                println!("  warning: {w}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("✗ {}", dir.display());
            for d in e.diagnostics() {
                println!("  {d}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

async fn local_doctor(home: &GroveHome) -> anyhow::Result<Vec<DoctorCheck>> {
    let mut opts = grove_core::CoreOptions::new(home.clone());
    opts.proxy = None;
    opts.reap_orphans = false;
    opts.idle = false;
    let core = grove_core::Core::start(opts).await?;
    let mut checks = core.doctor().await;
    for c in checks.iter_mut().filter(|c| c.name == "proxy") {
        c.detail = "Grove isn't running".into();
    }
    Ok(checks)
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn print_config_problems(status: &ConfigStatus) {
    match status {
        ConfigStatus::Loaded { warnings, .. } => {
            for w in warnings {
                eprintln!("warning: {}", w.message);
            }
        }
        ConfigStatus::NotConfigured => {
            eprintln!("Grove isn't configured yet. Open the app or write ~/.grove/config.toml.")
        }
        ConfigStatus::WorkspaceMissing { message } => eprintln!("{message}"),
        ConfigStatus::Invalid { diagnostics } => {
            eprintln!("Config problems:");
            for d in diagnostics {
                match &d.file {
                    Some(f) => eprintln!("  {}: {}", f.display(), d.message),
                    None => eprintln!("  {}", d.message),
                }
            }
        }
    }
}

fn origin_label(origin: &ClusterOrigin) -> String {
    match origin {
        ClusterOrigin::Default => "main clones".into(),
        ClusterOrigin::Branch { branch } => format!("branch {branch}"),
        ClusterOrigin::PullRequest {
            project, number, ..
        } => format!("{project} PR #{number}"),
        ClusterOrigin::Manual => "manual".into(),
    }
}

fn print_list(snap: &Snapshot) {
    println!("{:<24} {:<12} {:<24} URL", "CLUSTER", "STATE", "FROM");
    for c in &snap.clusters {
        let url = c.urls().first().map(|(_, u)| u.clone()).unwrap_or_default();
        println!(
            "{:<24} {:<12} {:<24} {}",
            c.name,
            c.phase.label(),
            origin_label(&c.origin),
            url
        );
    }
}

fn print_cluster(c: &ClusterView) {
    println!(
        "{} — {} ({})",
        c.name,
        c.phase.label(),
        origin_label(&c.origin)
    );
    if let grove_core::ClusterPhase::Error { message } = &c.phase {
        println!("  error: {message}");
    }
    for inst in &c.instances {
        let checkout = match &inst.checkout {
            CheckoutView::MainClone { path, exists } => format!(
                "main clone {}{}",
                path.display(),
                if *exists { "" } else { " (missing)" }
            ),
            CheckoutView::Worktree { branch, path, .. } => format!(
                "worktree {} at {}",
                branch.as_deref().unwrap_or("(detached)"),
                path.display()
            ),
            CheckoutView::Reuse => "reuses default".into(),
        };
        println!("  {} — {checkout}", inst.project);
        if let Some(db) = &inst.database {
            println!("    database {} ({:?})", db.name, db.mode);
        }
        if let Some(pr) = &inst.pr {
            println!("    PR #{} {} {}", pr.number, pr.title, pr.url);
        }
        for p in &inst.processes {
            let state = match &p.state {
                grove_core::ProcState::Failed { message } => format!("failed: {message}"),
                grove_core::ProcState::Exited { status, .. } => format!("exited: {status}"),
                s => s.label().to_string(),
            };
            let port = p.port.map(|p| format!(":{p}")).unwrap_or_default();
            let url = p.url.as_deref().unwrap_or("");
            println!("    {:<12} {:<10} {:<7} {url}", p.name, state, port);
        }
    }
}

fn print_plan(plan: &ClusterPlan) {
    println!("Cluster {} ({})", plan.name, origin_label(&plan.origin));
    for (project, p) in &plan.projects {
        let checkout = match &p.checkout {
            PlannedCheckout::Reuse => "reuse default".to_string(),
            PlannedCheckout::Worktree { source } => format!("worktree on {}", source.branch()),
            PlannedCheckout::Adopt { path, .. } => {
                format!("use existing worktree {}", path.display())
            }
        };
        let db = match p.db {
            Some(DbMode::Fresh) => ", fresh database",
            Some(DbMode::Shared) => ", shared database",
            None => "",
        };
        let pr =
            p.pr.as_ref()
                .map(|pr| format!(" — #{} {}", pr.number, pr.title))
                .unwrap_or_default();
        println!("  {project:<12} {checkout}{db}{pr}");
    }
    for w in &plan.warnings {
        println!("  ⚠ {w}");
    }
}
