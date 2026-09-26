//! GUI apps on macOS don't inherit the login shell's environment, so version
//! managers (mise, asdf, nvm) and direnv would be missing. Like Zed and VS
//! Code, we run the user's login shell interactively in the checkout and read
//! its environment back.

use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::ProcError;
use crate::sys;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
/// direnv can be slow on a cold cache (e.g. nix-direnv).
const DIRENV_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_GRACE: Duration = Duration::from_secs(2);
const MAX_STDERR: usize = 16 * 1024;
/// Set for the capture shell so rc files can skip slow or noisy setup.
const CAPTURE_FLAG: &str = "GROVE_ENV_CAPTURE";
const NOISE: [&str; 5] = ["PWD", "OLDPWD", "SHLVL", "_", CAPTURE_FLAG];

/// `$SHELL`, else the passwd entry, else `/bin/zsh`.
pub fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(passwd_shell)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

fn passwd_shell() -> Option<PathBuf> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut result = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the given sizes; on success `result`
    // points at `pwd`, whose strings live in `buf`.
    unsafe {
        let rc = libc::getpwuid_r(
            libc::getuid(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() || (*result).pw_shell.is_null() {
            return None;
        }
        let shell = CStr::from_ptr((*result).pw_shell).to_bytes();
        (!shell.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(shell)))
    }
}

/// The environment the user's login shell has in `dir`, including direnv.
pub async fn capture_shell_env(dir: &Path) -> Result<HashMap<String, String>, ProcError> {
    capture_shell_env_with(&user_shell(), dir).await
}

/// Runs `<shell> -l -i -c '…; /usr/bin/env -0'` in `dir` (15s timeout).
/// If `direnv` is on the captured PATH and `dir` or a parent has an
/// `.envrc`, `direnv export json` is merged in; a direnv failure (e.g. a
/// blocked `.envrc`) is logged and skipped. PWD, OLDPWD, SHLVL and `_` are
/// dropped.
pub async fn capture_shell_env_with(
    shell: &Path,
    dir: &Path,
) -> Result<HashMap<String, String>, ProcError> {
    let fail = |message: String| ProcError::ShellEnv {
        shell: shell.to_owned(),
        dir: dir.to_owned(),
        message,
    };
    let mut env = run_login_shell(shell, dir).await.map_err(fail)?;
    if let Some(direnv) = find_direnv(&env, dir) {
        match direnv_export(&direnv, dir, &env).await {
            Ok(changes) => apply_direnv(&mut env, changes),
            Err(err) => tracing::warn!(dir = %dir.display(), "direnv export failed: {err}"),
        }
    }
    for key in NOISE {
        env.remove(key);
    }
    Ok(env)
}

struct Markers {
    begin: String,
    end: String,
}

impl Markers {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let id = format!("__GROVE_ENV_{}_{nanos}", std::process::id());
        Self {
            begin: format!("{id}_BEGIN__"),
            end: format!("{id}_END__"),
        }
    }

    /// The bytes between the markers, once both have been printed.
    fn body<'a>(&self, out: &'a [u8]) -> Option<&'a [u8]> {
        let start = find(out, self.begin.as_bytes())? + self.begin.len();
        let len = find(&out[start..], self.end.as_bytes())?;
        Some(&out[start..start + len])
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn run_login_shell(shell: &Path, dir: &Path) -> Result<HashMap<String, String>, String> {
    let markers = Markers::new();
    // The explicit cd fires chpwd hooks (mise, asdf) in case rc files moved.
    let script = format!(
        "cd {}; printf '%s' '{}'; /usr/bin/env -0; printf '%s' '{}'",
        shell_quote(&dir.to_string_lossy()),
        markers.begin,
        markers.end,
    );
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", &script])
        .current_dir(dir)
        .env(CAPTURE_FLAG, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A new session without a controlling terminal: an interactive shell
    // must not grab the terminal Grove was started from (in dev), and it
    // makes the capture shell's leftovers one process group.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let pgid = child.id();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let mut out = Vec::new();
    let mut err = Vec::new();
    let read = read_until_end(&mut stdout, &mut stderr, &mut out, &mut err, &markers);
    let timed_out = timeout(CAPTURE_TIMEOUT, read).await.is_err();
    let status = if timed_out {
        None
    } else {
        timeout(EXIT_GRACE, child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    };
    // Background jobs started by rc files would otherwise leak, one set per
    // capture. SIGHUP is what they'd get from closing a terminal.
    if let Some(pgid) = pgid {
        let sig = if timed_out {
            libc::SIGKILL
        } else {
            libc::SIGHUP
        };
        sys::signal_group(pgid, sig);
    }

    if timed_out {
        return Err(format!(
            "timed out after {}s{}",
            CAPTURE_TIMEOUT.as_secs(),
            stderr_tail(&err)
        ));
    }
    match markers.body(&out) {
        Some(body) => Ok(parse_env(body)),
        None => {
            let status = status.map_or("did not exit".into(), |s| s.to_string());
            Err(format!(
                "the shell did not print its environment ({status}){}",
                stderr_tail(&err)
            ))
        }
    }
}

/// Reads stdout until the end marker (or EOF), collecting stderr alongside
/// so neither pipe fills up. Stopping at the marker means a background job
/// holding stdout open can't stall us.
async fn read_until_end(
    stdout: &mut (impl AsyncRead + Unpin),
    stderr: &mut (impl AsyncRead + Unpin),
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    markers: &Markers,
) {
    let mut obuf = [0u8; 8192];
    let mut ebuf = [0u8; 4096];
    let mut stderr_open = true;
    loop {
        tokio::select! {
            n = stdout.read(&mut obuf) => match n {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    out.extend_from_slice(&obuf[..n]);
                    if markers.body(out).is_some() {
                        return;
                    }
                }
            },
            n = stderr.read(&mut ebuf), if stderr_open => match n {
                Ok(0) | Err(_) => stderr_open = false,
                Ok(n) => {
                    err.extend_from_slice(&ebuf[..n]);
                    if err.len() > MAX_STDERR {
                        err.drain(..err.len() - MAX_STDERR);
                    }
                }
            },
        }
    }
}

fn stderr_tail(err: &[u8]) -> String {
    let text = String::from_utf8_lossy(err);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let tail: Vec<&str> = text.lines().rev().take(10).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    format!(":\n{}", tail.join("\n"))
}

/// Parses `env -0` output: `KEY=value` entries separated by NULs.
fn parse_env(body: &[u8]) -> HashMap<String, String> {
    body.split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let (key, value) = entry.split_once('=')?;
            (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn find_direnv(env: &HashMap<String, String>, dir: &Path) -> Option<PathBuf> {
    if !dir.ancestors().any(|d| d.join(".envrc").is_file()) {
        return None;
    }
    std::env::split_paths(env.get("PATH")?)
        .map(|p| p.join("direnv"))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

async fn direnv_export(
    direnv: &Path,
    dir: &Path,
    env: &HashMap<String, String>,
) -> Result<HashMap<String, Option<String>>, String> {
    let output = Command::new(direnv)
        .args(["export", "json"])
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = timeout(DIRENV_TIMEOUT, output)
        .await
        .map_err(|_| format!("timed out after {}s", DIRENV_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("{}{}", output.status, stderr_tail(&output.stderr)));
    }
    parse_direnv(&output.stdout)
}

/// `direnv export json` prints nothing when there is nothing to change.
fn parse_direnv(stdout: &[u8]) -> Result<HashMap<String, Option<String>>, String> {
    if stdout.trim_ascii().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_slice(stdout).map_err(|e| format!("unexpected output: {e}"))
}

fn apply_direnv(env: &mut HashMap<String, String>, changes: HashMap<String, Option<String>>) {
    for (key, value) in changes {
        match value {
            Some(value) => env.insert(key, value),
            None => env.remove(&key),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_shell_is_absolute() {
        assert!(user_shell().is_absolute());
        assert!(passwd_shell().is_some_and(|s| s.is_absolute()));
    }

    #[test]
    fn extracts_env_between_markers() {
        let m = Markers::new();
        let mut out = b"motd junk\n".to_vec();
        out.extend_from_slice(m.begin.as_bytes());
        out.extend_from_slice(b"A=1\0MULTI=x\ny=z\0EMPTY=\0junk\0=bad\0");
        out.extend_from_slice(m.end.as_bytes());
        out.extend_from_slice(b"logout noise");
        let env = parse_env(m.body(&out).unwrap());
        assert_eq!(env.len(), 3);
        assert_eq!(env["A"], "1");
        assert_eq!(env["MULTI"], "x\ny=z");
        assert_eq!(env["EMPTY"], "");

        assert!(m.body(m.end.as_bytes()).is_none());
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn merges_direnv() {
        let mut env = HashMap::from([
            ("KEEP".to_owned(), "1".to_owned()),
            ("GONE".to_owned(), "1".to_owned()),
        ]);
        let changes = parse_direnv(br#"{"GONE": null, "NEW": "2", "KEEP": "3"}"#).unwrap();
        apply_direnv(&mut env, changes);
        assert_eq!(
            env,
            HashMap::from([
                ("KEEP".to_owned(), "3".to_owned()),
                ("NEW".to_owned(), "2".to_owned()),
            ])
        );
        assert!(parse_direnv(b"\n").unwrap().is_empty());
        assert!(parse_direnv(b"nope").is_err());
    }

    #[test]
    fn direnv_needs_an_envrc() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let direnv = bin.join("direnv");
        std::fs::write(&direnv, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&direnv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = HashMap::from([("PATH".to_owned(), bin.display().to_string())]);

        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        assert_eq!(find_direnv(&env, &project), None);
        std::fs::write(dir.path().join(".envrc"), "").unwrap();
        assert_eq!(find_direnv(&env, &project), Some(direnv));
    }

    #[tokio::test]
    async fn captures_the_login_shell_env() {
        let dir = tempfile::tempdir().unwrap();
        let env = timeout(Duration::from_secs(30), capture_shell_env(dir.path()))
            .await
            .expect("capture should finish")
            .unwrap();
        assert!(env.get("PATH").is_some_and(|p| p.contains("/usr/bin")));
        assert!(env.get("HOME").is_some_and(|h| !h.is_empty()));
        for key in NOISE {
            assert!(!env.contains_key(key), "{key} should be dropped");
        }
    }

    #[tokio::test]
    async fn captures_with_sh() {
        let dir = tempfile::tempdir().unwrap();
        let env = capture_shell_env_with(Path::new("/bin/sh"), dir.path())
            .await
            .unwrap();
        assert!(env.contains_key("PATH"));
    }

    #[tokio::test]
    async fn reports_a_broken_shell() {
        let dir = tempfile::tempdir().unwrap();
        let err = capture_shell_env_with(Path::new("/usr/bin/false"), dir.path())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("did not print its environment"),
            "{err}"
        );
        let err = capture_shell_env_with(Path::new("/no/such/shell"), dir.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not start"), "{err}");
    }
}
