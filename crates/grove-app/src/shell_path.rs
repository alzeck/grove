//! Apps started from Finder get a minimal PATH, so Homebrew's git/gh/pg
//! tools would be missing. Borrow PATH from the user's login shell.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "__GROVE_PATH__";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Must run before any other thread starts (it calls `set_var`).
pub fn adopt_login_shell_path() {
    // A terminal already gave us the right PATH.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return;
    }
    let Some(path) = login_shell_path() else {
        return;
    };
    // SAFETY: called at the top of main, before any threads are spawned.
    unsafe { std::env::set_var("PATH", path) };
}

fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("printf '{MARKER}%s{MARKER}' \"$PATH\""),
        ])
        .current_dir(home)
        .env("GROVE_ENV_CAPTURE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let start = out.find(MARKER)? + MARKER.len();
    let len = out[start..].find(MARKER)?;
    let path = out[start..start + len].trim().to_string();
    (!path.is_empty()).then_some(path)
}
