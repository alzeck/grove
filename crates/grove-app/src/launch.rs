//! Finding or starting the app for CLI commands.

use grove_config::GroveHome;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Makes sure the app answers on its socket, starting it in the background
/// (window hidden) if needed.
pub async fn ensure_running(home: &GroveHome) -> anyhow::Result<()> {
    let socket = home.socket_path();
    if grove_ipc::is_running(&socket).await {
        return Ok(());
    }
    eprintln!("Starting Grove in the background…");
    spawn_background(home)?;
    let start = Instant::now();
    while start.elapsed() < START_TIMEOUT {
        if grove_ipc::is_running(&socket).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "Grove didn't start within {}s; see {}",
        START_TIMEOUT.as_secs(),
        launch_log(home).display()
    )
}

fn launch_log(home: &GroveHome) -> std::path::PathBuf {
    home.logs_dir().join("grove.log")
}

fn spawn_background(home: &GroveHome) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(home.logs_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(launch_log(home))?;
    Command::new(exe)
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own process group, so it outlives this terminal.
        .process_group(0)
        .spawn()?;
    Ok(())
}
