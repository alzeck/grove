//! End-to-end: real git repos, real processes (the demo's Python servers),
//! the real proxy on a random port, driven through `Core`.

use grove_config::GroveHome;
use grove_core::{
    ClusterPhase, Core, CoreOptions, NewClusterSource, PlannedCheckout, ProcState, ProxyStatus,
    TeardownOptions,
};
use grove_proxy::ProxyConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const API_SERVER: &str = include_str!("../../../examples/demo/api/server.py");
const WEB_SERVER: &str = include_str!("../../../examples/demo/web/server.py");

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=grove",
            "-c",
            "user.email=grove@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: GroveHome,
    clones: PathBuf,
}

/// Two repos (api, web) with a `feature/x` branch on web, a workspace
/// describing them, and a Grove home using it.
fn fixture(idle_timeout: &str, port_range: [u16; 2]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical path: macOS tempdirs live behind the /var → /private/var symlink.
    let root = tmp.path().canonicalize().unwrap();
    let clones = root.join("clones");
    for (app, server) in [("api", API_SERVER), ("web", WEB_SERVER)] {
        let origin = root.join(format!("origin/{app}.git"));
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let clone = clones.join(app);
        std::fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(
            &clone,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(clone.join("server.py"), server).unwrap();
        std::fs::write(clone.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(clone.join(".env"), "SECRET=main\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-q", "-m", "init"]);
        git(&clone, &["push", "-q", "-u", "origin", "main"]);
    }
    let web = clones.join("web");
    git(&web, &["checkout", "-q", "-b", "feature/x"]);
    let server = std::fs::read_to_string(web.join("server.py"))
        .unwrap()
        .replace("TITLE = \"Grove demo\"", "TITLE = \"Feature X\"");
    std::fs::write(web.join("server.py"), server).unwrap();
    git(&web, &["commit", "-q", "-am", "feature x"]);
    git(&web, &["push", "-q", "-u", "origin", "feature/x"]);
    git(&web, &["checkout", "-q", "main"]);

    let ws = root.join("workspace");
    std::fs::create_dir_all(ws.join("projects")).unwrap();
    std::fs::write(
        ws.join("grove.toml"),
        format!("name = \"e2e\"\nidle_timeout = \"{idle_timeout}\"\n"),
    )
    .unwrap();
    std::fs::write(
        ws.join("projects/api.toml"),
        format!(
            r#"name = "api"
repo = "{}"
copy_from_original = [".env"]
[processes.server]
run = "python3 -u server.py"
port = "PORT"
ready = {{ http = "/health", timeout = "20s" }}
domain = true
"#,
            root.join("origin/api.git").display()
        ),
    )
    .unwrap();
    std::fs::write(
        ws.join("projects/web.toml"),
        format!(
            r#"name = "web"
repo = "{}"
copy_from_original = [".env"]
[env]
API_URL = "{{{{ projects.api.server.url }}}}"
API_LOCAL_URL = "{{{{ projects.api.server.local_url }}}}"
[processes.app]
run = "python3 -u server.py"
port = "PORT"
ready = {{ http = "/", timeout = "20s" }}
domain = {{ wildcard = true }}
depends_on = ["api.server"]
"#,
            root.join("origin/web.git").display()
        ),
    )
    .unwrap();

    let home = GroveHome::new(root.join("home"));
    std::fs::create_dir_all(home.root()).unwrap();
    std::fs::write(
        home.config_file(),
        format!(
            "workspace = \"{}\"\nclones_root = \"{}\"\nport_range = [{}, {}]\n",
            ws.display(),
            clones.display(),
            port_range[0],
            port_range[1]
        ),
    )
    .unwrap();
    Fixture {
        _tmp: tmp,
        home,
        clones,
    }
}

async fn start_core(home: &GroveHome, idle_tick: Duration) -> Core {
    let mut opts = CoreOptions::new(home.clone());
    opts.proxy = Some(ProxyConfig {
        https_addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        http_addrs: vec![],
        ready_timeout: Duration::from_secs(30),
    });
    opts.idle_tick = idle_tick;
    Core::start(opts).await.expect("core starts")
}

fn proxy_port(core: &Core) -> u16 {
    match core.snapshot().proxy {
        ProxyStatus::Running { https } => https[0].rsplit(':').next().unwrap().parse().unwrap(),
        other => panic!("proxy not running: {other:?}"),
    }
}

/// GET through the proxy with curl (trusting Grove's CA).
async fn fetch(core: &Core, host: &str) -> String {
    let port = proxy_port(core);
    let ca = core.ca_cert_path().unwrap();
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let out = Command::new("curl")
            .args(["-s", "--max-time", "30", "--cacert"])
            .arg(ca)
            .arg("--resolve")
            .arg(format!("{host}:{port}:127.0.0.1"))
            .arg(format!("https://{host}:{port}/"))
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    })
    .await
    .unwrap()
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn proc_state(core: &Core, cluster: &str, project: &str, process: &str) -> ProcState {
    core.snapshot()
        .cluster(cluster)
        .and_then(|c| c.instance(project))
        .and_then(|i| i.processes.iter().find(|p| p.name == process))
        .map(|p| p.state.clone())
        .unwrap_or_default()
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clusters_end_to_end() {
    let fx = fixture("0", [4700, 4749]);
    let core = start_core(&fx.home, Duration::from_secs(15)).await;

    // Default cluster from the main clones.
    core.start_cluster("default").await.expect("default starts");
    assert_eq!(
        proc_state(&core, "default", "api", "server"),
        ProcState::Ready
    );
    assert_eq!(proc_state(&core, "default", "web", "app"), ProcState::Ready);
    let page = fetch(&core, "web.localhost").await;
    assert!(page.contains("<b>default</b>"), "{page}");
    assert!(page.contains("hello from the api"), "{page}");
    // Wildcard subdomain.
    assert!(
        fetch(&core, "tenant.web.localhost")
            .await
            .contains("Grove demo")
    );

    // A branch that only exists in web: web gets a worktree, api is reused.
    let plan = core
        .plan_cluster(NewClusterSource::Branch {
            branch: "feature/x".into(),
            project: None,
            base: None,
        })
        .await
        .expect("plan");
    assert_eq!(plan.name, "feature-x");
    assert_eq!(plan.projects["api"].checkout, PlannedCheckout::Reuse);
    assert!(matches!(
        plan.projects["web"].checkout,
        PlannedCheckout::Worktree { .. }
    ));
    core.create_cluster(plan).await.expect("create");
    assert_eq!(core.phase("feature-x"), ClusterPhase::Running);

    let worktree = fx.home.worktree_path("feature-x", "web");
    assert_eq!(
        std::fs::read_to_string(worktree.join(".env")).unwrap(),
        "SECRET=main\n",
        "copy_from_original"
    );
    let page = fetch(&core, "web-feature-x.localhost").await;
    assert!(page.contains("Feature X"), "{page}");
    assert!(page.contains("<b>feature-x</b>"), "{page}");
    // Reused api: the feature cluster talks to the default api.
    assert!(
        page.contains("&quot;cluster&quot;: &quot;default&quot;"),
        "{page}"
    );

    // Restart gives a new process.
    let pid = core
        .process_handle("feature-x", "web", "app")
        .unwrap()
        .pid();
    core.restart_process("feature-x", "web", "app")
        .await
        .expect("restart");
    let new_pid = core
        .process_handle("feature-x", "web", "app")
        .unwrap()
        .pid();
    assert_ne!(pid, new_pid);
    assert!(!pid_alive(pid));

    // A crash is noticed.
    Command::new("kill")
        .args(["-9", &new_pid.to_string()])
        .status()
        .unwrap();
    wait_until("crash to be recorded", || {
        matches!(
            proc_state(&core, "feature-x", "web", "app"),
            ProcState::Exited { success: false, .. }
        )
    })
    .await;
    assert_eq!(core.phase("feature-x"), ClusterPhase::Degraded);

    // Teardown refuses dirty worktrees unless forced, and keeps the branch.
    std::fs::write(worktree.join("scratch.txt"), "wip").unwrap();
    let report = core.teardown_report("feature-x").await.unwrap();
    assert!(report.has_dirty_worktrees());
    assert!(
        core.teardown("feature-x", TeardownOptions::default())
            .await
            .is_err()
    );
    core.teardown(
        "feature-x",
        TeardownOptions {
            force: true,
            remove_adopted: false,
        },
    )
    .await
    .expect("forced teardown");
    assert!(!worktree.exists());
    assert!(core.snapshot().cluster("feature-x").is_none());
    let branches = Command::new("git")
        .arg("-C")
        .arg(fx.clones.join("web"))
        .args(["branch", "--list", "feature/x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains("feature/x"));

    // Re-creating the same branch works (stale worktree metadata is pruned).
    let plan = core
        .plan_cluster(NewClusterSource::Branch {
            branch: "feature/x".into(),
            project: None,
            base: None,
        })
        .await
        .unwrap();
    let mut plan = plan;
    plan.start = false;
    core.create_cluster(plan).await.expect("re-create");
    assert_eq!(core.phase("feature-x"), ClusterPhase::Stopped);

    // State survives a restart; orphans left by a "crashed" core are reaped.
    let api_pid = core
        .process_handle("default", "api", "server")
        .unwrap()
        .pid();
    let second = start_core(&fx.home, Duration::from_secs(15)).await;
    assert!(second.snapshot().cluster("feature-x").is_some());
    wait_until("orphan to be reaped", || !pid_alive(api_pid)).await;

    second.shutdown().await;
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_clusters_wake_on_request() {
    let fx = fixture("1s", [4750, 4799]);
    let core = start_core(&fx.home, Duration::from_millis(200)).await;

    let mut plan = core
        .plan_cluster(NewClusterSource::Branch {
            branch: "feature/x".into(),
            project: None,
            base: None,
        })
        .await
        .unwrap();
    plan.start = true;
    core.create_cluster(plan).await.expect("create");

    wait_until("cluster to idle", || {
        core.phase("feature-x") == ClusterPhase::Idle
    })
    .await;
    assert!(core.process_handle("feature-x", "web", "app").is_none());

    // A non-browser request waits for the wake-up and gets the real page.
    let page = fetch(&core, "web-feature-x.localhost").await;
    assert!(page.contains("Feature X"), "{page}");
    assert!(core.phase("feature-x").is_active());

    core.shutdown().await;
}
