use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, watch};

use crate::error::ProcError;
use crate::exit::{ExitInfo, ProcStatus};
use crate::log::LogFile;
use crate::orphans::{PidRecord, process_start_time};
use crate::replay::Replay;
use crate::shell_env::user_shell;
use crate::sys;

pub const DEFAULT_REPLAY_BYTES: usize = 1024 * 1024;
pub const DEFAULT_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const BROADCAST_CAPACITY: usize = 1024;
const READ_CHUNK: usize = 16 * 1024;
const DEFAULT_TERM: &str = "xterm-256color";
const GROUP_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Shell command line, run as `<shell> -c <command>`.
    pub command: String,
    pub cwd: PathBuf,
    /// The child's complete environment; nothing else is inherited. `TERM`
    /// defaults to `xterm-256color` when missing, and portable-pty fills in
    /// `SHELL` when missing.
    pub env: HashMap<String, String>,
    pub shell: PathBuf,
    pub log_path: Option<PathBuf>,
    /// The log moves to `<log>.1` before it grows past this many bytes.
    pub log_rotate_bytes: u64,
    pub cols: u16,
    pub rows: u16,
    /// How much recent output new subscribers get replayed.
    pub replay_bytes: usize,
}

impl SpawnSpec {
    /// Runs `command` with the user's shell and this process's environment.
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            env: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
            shell: user_shell(),
            log_path: None,
            log_rotate_bytes: DEFAULT_LOG_ROTATE_BYTES,
            cols: 120,
            rows: 32,
            replay_bytes: DEFAULT_REPLAY_BYTES,
        }
    }
}

/// Recent output plus a live stream that continues exactly where the replay
/// ends. `rx` reports `Closed` once the PTY reaches EOF and every chunk has
/// been delivered. A receiver that falls more than ~1024 chunks behind gets
/// `Lagged`; subscribe again to resync from a fresh replay.
#[derive(Debug)]
pub struct OutputSubscription {
    pub replay: Bytes,
    pub rx: broadcast::Receiver<Bytes>,
}

/// A command running in its own PTY as a session and process-group leader
/// (so `pid` is also the process-group id). Clones share the same process.
/// Dropping every handle leaves the process running; output keeps being
/// drained into the replay and log until it exits.
#[derive(Clone)]
pub struct ManagedProcess {
    inner: Arc<Inner>,
}

struct Inner {
    pid: u32,
    start_time: Option<u64>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// A dup of the master fd. portable-pty's own writer sends EOF to the
    /// child when dropped, which would end programs that read stdin.
    writer: Mutex<File>,
    shared: Arc<Shared>,
}

/// State shared with the reader and waiter threads.
struct Shared {
    output: Mutex<Output>,
    exit: watch::Sender<Option<ExitInfo>>,
}

struct Output {
    replay: Replay,
    /// `None` once the PTY reached EOF.
    tx: Option<broadcast::Sender<Bytes>>,
}

impl ManagedProcess {
    pub fn spawn(spec: SpawnSpec) -> Result<Self, ProcError> {
        // portable-pty silently falls back to $HOME for a missing cwd.
        if !spec.cwd.is_dir() {
            return Err(ProcError::Cwd(spec.cwd));
        }
        let log = match &spec.log_path {
            Some(path) => Some(
                LogFile::open(path, spec.log_rotate_bytes).map_err(|source| ProcError::Log {
                    path: path.clone(),
                    source,
                })?,
            ),
            None => None,
        };

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ProcError::Pty(format!("{e:#}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ProcError::Pty(format!("{e:#}")))?;
        let writer = dup_master(&*pair.master)?;

        let mut cmd = CommandBuilder::new(&spec.shell);
        cmd.args(["-c", &spec.command]);
        cmd.cwd(&spec.cwd);
        cmd.env_clear();
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        if !spec.env.contains_key("TERM") {
            cmd.env("TERM", DEFAULT_TERM);
        }

        // portable-pty runs setsid() + TIOCSCTTY in the child, making it a
        // session and process-group leader with the PTY as its terminal.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ProcError::Spawn {
                command: spec.command.clone(),
                message: format!("{e:#}"),
            })?;
        // Our copy of the slave must go, or the reader never sees EOF.
        drop(pair.slave);
        let pid = child.process_id().ok_or_else(|| ProcError::Spawn {
            command: spec.command.clone(),
            message: "no process id".into(),
        })?;
        // We reap with waitpid ourselves to keep the signal number, which
        // portable-pty's ExitStatus loses. Dropping std's Child is a no-op.
        drop(child);
        // Read before the waiter can reap it, so the pid can't be reused yet.
        let start_time = process_start_time(pid);

        let shared = Arc::new(Shared {
            output: Mutex::new(Output {
                replay: Replay::new(spec.replay_bytes),
                tx: Some(broadcast::channel(BROADCAST_CAPACITY).0),
            }),
            exit: watch::channel(None).0,
        });
        if let Err(err) = start_threads(pid, reader, log, &shared) {
            sys::signal_group(pid, libc::SIGKILL);
            return Err(err.into());
        }

        Ok(Self {
            inner: Arc::new(Inner {
                pid,
                start_time,
                master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                shared,
            }),
        })
    }

    /// Also the process-group id.
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Kernel start time (seconds since the epoch), for [`PidRecord`]. `None`
    /// off macOS or if the process was already gone.
    pub fn start_time(&self) -> Option<u64> {
        self.inner.start_time
    }

    /// What to persist so a later [`crate::reap_orphans`] can clean up.
    pub fn pid_record(&self) -> Option<PidRecord> {
        Some(PidRecord {
            pid: self.inner.pid,
            start_time: self.inner.start_time?,
        })
    }

    pub fn status(&self) -> ProcStatus {
        match *self.inner.shared.exit.borrow() {
            Some(info) => ProcStatus::Exited(info),
            None => ProcStatus::Running,
        }
    }

    /// Takes the replay and subscribes atomically: no gap, no duplicates.
    pub fn subscribe(&self) -> OutputSubscription {
        let output = self.inner.shared.output.lock();
        let rx = match &output.tx {
            Some(tx) => tx.subscribe(),
            // Already at EOF: hand out a receiver that reports Closed.
            None => broadcast::channel(1).1,
        };
        OutputSubscription {
            replay: output.replay.snapshot(),
            rx,
        }
    }

    /// Writes to the child's terminal input. May block briefly if the child
    /// isn't reading.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.inner.writer.lock().write_all(data)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.inner
            .master
            .lock()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    pub async fn wait(&self) -> ExitInfo {
        let mut rx = self.inner.shared.exit.subscribe();
        // Copy out of the watch guard before any other await: holding it
        // would make this future !Send.
        let result = rx
            .wait_for(Option::is_some)
            .await
            .map(|info| info.unwrap_or_default());
        match result {
            Ok(info) => info,
            // The sender lives in `shared`, which `self` keeps alive.
            Err(_) => std::future::pending().await,
        }
    }

    /// SIGTERM to the whole process group, then SIGKILL if the leader or any
    /// other member is still around after `grace`. Returns immediately if the
    /// process has already exited.
    pub async fn terminate(&self, grace: Duration) -> ExitInfo {
        if let ProcStatus::Exited(info) = self.status() {
            return info;
        }
        let pgid = self.inner.pid;
        sys::signal_group(pgid, libc::SIGTERM);
        let stopped = async {
            let info = self.wait().await;
            wait_group_gone(pgid).await;
            info
        };
        if let Ok(info) = tokio::time::timeout(grace, stopped).await {
            return info;
        }
        sys::signal_group(pgid, libc::SIGKILL);
        let info = self.wait().await;
        // Members reparented to launchd are reaped shortly after SIGKILL.
        let _ = tokio::time::timeout(Duration::from_secs(1), wait_group_gone(pgid)).await;
        info
    }
}

impl fmt::Debug for ManagedProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedProcess")
            .field("pid", &self.inner.pid)
            .field("status", &self.status())
            .finish()
    }
}

async fn wait_group_gone(pgid: u32) {
    while sys::group_exists(pgid) {
        tokio::time::sleep(GROUP_POLL).await;
    }
}

fn dup_master(master: &dyn MasterPty) -> Result<File, ProcError> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| ProcError::Pty("PTY master has no file descriptor".into()))?;
    // SAFETY: `fd` belongs to `master`, which outlives this borrow.
    let owned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
    Ok(File::from(owned))
}

/// The waiter goes first so the child is always reaped even if the reader
/// can't start.
fn start_threads(
    pid: u32,
    reader: Box<dyn Read + Send>,
    log: Option<LogFile>,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    let waiter = Arc::clone(shared);
    thread::Builder::new()
        .name(format!("grove-proc-wait-{pid}"))
        .spawn(move || {
            let info = sys::wait_pid(pid);
            waiter.exit.send_replace(Some(info));
        })?;
    let drainer = Arc::clone(shared);
    thread::Builder::new()
        .name(format!("grove-proc-read-{pid}"))
        .spawn(move || read_output(pid, reader, log, &drainer))?;
    Ok(())
}

/// Drains the PTY until EOF (every process holding the terminal is gone).
/// Holding the reader keeps the master open, so the child doesn't get a
/// hangup when the last handle is dropped.
fn read_output(
    pid: u32,
    mut reader: Box<dyn Read + Send>,
    mut log: Option<LogFile>,
    shared: &Shared,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::debug!(pid, %err, "PTY read failed");
                break;
            }
        };
        let chunk = Bytes::copy_from_slice(&buf[..n]);
        {
            let mut output = shared.output.lock();
            output.replay.push(&chunk);
            if let Some(tx) = &output.tx {
                // No receivers is fine.
                let _ = tx.send(chunk.clone());
            }
        }
        if let Some(file) = log.as_mut()
            && let Err(err) = file.write(&chunk)
        {
            tracing::warn!(pid, %err, "writing the process log failed; logging stopped");
            log = None;
        }
    }
    shared.output.lock().tx = None;
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Instant;

    use tokio::sync::broadcast::error::RecvError;
    use tokio::time::timeout;

    use super::*;

    pub(crate) fn sh(command: &str) -> SpawnSpec {
        let mut spec = SpawnSpec::new(command, std::env::temp_dir());
        spec.shell = "/bin/sh".into();
        spec
    }

    /// SIGKILLs the group on drop so a failing test doesn't leak processes.
    pub(crate) struct Guard(pub ManagedProcess);

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.0.status() == ProcStatus::Running {
                sys::signal_group(self.0.pid(), libc::SIGKILL);
            }
        }
    }

    pub(crate) fn spawn(spec: SpawnSpec) -> Guard {
        Guard(ManagedProcess::spawn(spec).unwrap())
    }

    /// Everything the process printed, once the PTY reaches EOF.
    pub(crate) async fn all_output(p: &ManagedProcess) -> String {
        let OutputSubscription { replay, mut rx } = p.subscribe();
        let collect = async move {
            let mut out = replay.to_vec();
            loop {
                match rx.recv().await {
                    Ok(chunk) => out.extend_from_slice(&chunk),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break out,
                }
            }
        };
        let out = timeout(Duration::from_secs(10), collect)
            .await
            .expect("output should end");
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Output so far, once it contains `needle`.
    pub(crate) async fn output_until(p: &ManagedProcess, needle: &str) -> String {
        let OutputSubscription { replay, mut rx } = p.subscribe();
        let collect = async move {
            let mut out = replay.to_vec();
            loop {
                let text = String::from_utf8_lossy(&out).into_owned();
                if text.contains(needle) {
                    break text;
                }
                match rx.recv().await {
                    Ok(chunk) => out.extend_from_slice(&chunk),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => panic!("output ended without {needle:?}: {text}"),
                }
            }
        };
        timeout(Duration::from_secs(10), collect)
            .await
            .unwrap_or_else(|_| panic!("no {needle:?} in the output"))
    }

    async fn wait(p: &ManagedProcess) -> ExitInfo {
        timeout(Duration::from_secs(10), p.wait())
            .await
            .expect("process should exit")
    }

    #[test]
    fn handles_are_send_sync() {
        fn check<T: Send + Sync + Clone>() {}
        check::<ManagedProcess>();
    }

    /// Compile-time only: grove-core runs these inside `tokio::spawn`.
    #[allow(dead_code, clippy::let_underscore_future)]
    fn futures_are_send(p: &ManagedProcess, check: &grove_config::ReadyCheck) {
        fn send<T: Send>(_: T) {}
        send(p.wait());
        send(p.terminate(Duration::ZERO));
        send(crate::wait_ready(check, None, p));
        send(crate::reap_orphans(&[], Duration::ZERO));
        send(crate::capture_shell_env(Path::new("/")));
    }

    #[tokio::test]
    async fn echo_shows_up_in_the_replay() {
        let p = spawn(sh("echo hi"));
        let info = wait(&p.0).await;
        assert!(info.success(), "{info}");
        assert_eq!(p.0.status(), ProcStatus::Exited(info));
        assert!(all_output(&p.0).await.contains("hi\r\n"));
        // Subscribing after EOF still replays and then reports Closed.
        let sub = p.0.subscribe();
        assert!(String::from_utf8_lossy(&sub.replay).contains("hi"));
    }

    #[tokio::test]
    async fn gets_exactly_the_given_env() {
        let mut spec =
            sh("echo \"foo=[$GROVE_TEST_FOO] term=[$TERM] manifest=[$CARGO_MANIFEST_DIR]\"");
        spec.env = HashMap::from([
            ("GROVE_TEST_FOO".into(), "bar baz".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]);
        let p = spawn(spec);
        let out = all_output(&p.0).await;
        assert!(
            out.contains("foo=[bar baz] term=[xterm-256color] manifest=[]"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn runs_in_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sh("pwd -P");
        spec.cwd = dir.path().to_owned();
        let p = spawn(spec);
        let out = all_output(&p.0).await;
        let expected = fs::canonicalize(dir.path()).unwrap();
        assert!(out.contains(expected.to_str().unwrap()), "{out}");
    }

    #[test]
    fn missing_cwd_is_an_error() {
        let mut spec = sh("true");
        spec.cwd = "/definitely/not/here".into();
        assert!(matches!(
            ManagedProcess::spawn(spec),
            Err(ProcError::Cwd(_))
        ));
    }

    #[tokio::test]
    async fn missing_shell_is_an_error() {
        let mut spec = sh("true");
        spec.shell = "/definitely/not/a/shell".into();
        let err = ManagedProcess::spawn(spec).unwrap_err();
        assert!(matches!(err, ProcError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn exit_codes_and_signals() {
        let p = spawn(sh("exit 3"));
        let info = wait(&p.0).await;
        assert_eq!(info.code, Some(3));
        assert_eq!(info.to_string(), "exit code 3");

        let p = spawn(sh("kill -TERM $$"));
        let info = wait(&p.0).await;
        assert_eq!(info.signal, Some(libc::SIGTERM));
        assert_eq!(info.to_string(), "killed by SIGTERM");
    }

    #[tokio::test]
    async fn many_waiters() {
        let p = spawn(sh("sleep 0.2; exit 7"));
        let a = p.0.clone();
        let b = p.0.clone();
        let (x, y) = tokio::join!(a.wait(), b.wait());
        assert_eq!(x.code, Some(7));
        assert_eq!(x, y);
    }

    #[tokio::test]
    async fn runs_as_group_leader() {
        let p = spawn(sh("sleep 5"));
        let pid = p.0.pid();
        assert!(sys::is_group_leader(pid));
        // SAFETY: plain syscall.
        assert_eq!(
            unsafe { libc::getsid(pid as libc::pid_t) },
            pid as libc::pid_t
        );
        p.0.terminate(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn terminate_kills_the_whole_tree() {
        let p = spawn(sh(
            "sleep 100 & echo \"kid:$!\"; sleep 100 & echo \"kid:$!\"; echo armed; wait",
        ));
        let out = output_until(&p.0, "armed").await;
        let kids: Vec<u32> = out
            .lines()
            .filter_map(|l| l.trim().strip_prefix("kid:")?.parse().ok())
            .collect();
        assert_eq!(kids.len(), 2, "{out}");
        assert!(kids.iter().all(|&k| sys::pid_alive(k)));

        let started = Instant::now();
        let info = p.0.terminate(Duration::from_secs(5)).await;
        assert_eq!(info.signal, Some(libc::SIGTERM), "{info}");
        assert!(started.elapsed() < Duration::from_secs(4));

        for kid in kids {
            let deadline = Instant::now() + Duration::from_secs(5);
            while sys::pid_alive(kid) {
                assert!(Instant::now() < deadline, "grandchild {kid} survived");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // Idempotent, and immediate once exited.
        assert_eq!(p.0.terminate(Duration::from_secs(5)).await, info);
    }

    #[tokio::test]
    async fn terminate_escalates_to_sigkill() {
        let p = spawn(sh("trap '' TERM; echo armed; sleep 100"));
        output_until(&p.0, "armed").await;
        let info = p.0.terminate(Duration::from_millis(300)).await;
        assert_eq!(info.signal, Some(libc::SIGKILL), "{info}");
    }

    #[tokio::test]
    async fn dropping_handles_does_not_kill() {
        let p = ManagedProcess::spawn(sh("echo armed; sleep 100")).unwrap();
        output_until(&p, "armed").await;
        let pid = p.pid();
        drop(p);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(sys::pid_alive(pid));
        sys::signal_group(pid, libc::SIGKILL);
    }

    #[tokio::test]
    async fn stdin_reaches_the_child() {
        let p = spawn(sh("head -1"));
        p.0.write(b"hello grove\n").unwrap();
        assert!(wait(&p.0).await.success());
        assert!(all_output(&p.0).await.contains("hello grove"));
    }

    #[tokio::test]
    async fn resize_reaches_the_child() {
        let p = spawn(sh(
            "stty size; trap 'stty size; exit 0' WINCH; echo armed; while :; do sleep 0.05; done",
        ));
        let out = output_until(&p.0, "armed").await;
        assert!(out.contains("32 120"), "{out}");
        p.0.resize(100, 50).unwrap();
        assert!(wait(&p.0).await.success());
        assert!(all_output(&p.0).await.contains("50 100"));
    }

    #[tokio::test]
    async fn resize_after_exit_does_not_panic() {
        let p = spawn(sh("true"));
        wait(&p.0).await;
        let _ = p.0.resize(10, 10);
    }

    #[tokio::test]
    async fn subscribers_get_everything_once() {
        let p = spawn(sh("for i in 1 2 3 4 5; do echo line-$i; sleep 0.05; done"));
        // Subscribe mid-stream: replay + stream must be exactly the output.
        output_until(&p.0, "line-2").await;
        let mid = all_output(&p.0).await;
        let full = all_output(&p.0).await;
        assert_eq!(mid, full);
        for i in 1..=5 {
            assert_eq!(full.matches(&format!("line-{i}\r\n")).count(), 1, "{full}");
        }
    }

    #[tokio::test]
    async fn replay_is_capped() {
        let mut spec = sh("printf 'aaaaaaaaaa'; printf 'bbbbbbbbbb'");
        spec.replay_bytes = 8;
        let p = spawn(spec);
        wait(&p.0).await;
        all_output(&p.0).await;
        assert_eq!(&p.0.subscribe().replay[..], b"bbbbbbbb");
    }

    #[tokio::test]
    async fn writes_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/default/api/web.log");
        let run = |cmd: &str, log: &Path| {
            let mut spec = sh(cmd);
            spec.log_path = Some(log.to_owned());
            spawn(spec)
        };
        let p = run("echo first-run", &log);
        all_output(&p.0).await;
        let p = run("echo second-run", &log);
        all_output(&p.0).await;

        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.matches("── started ").count(), 2, "{text}");
        assert!(text.find("first-run").unwrap() < text.find("second-run").unwrap());
    }

    #[tokio::test]
    async fn rotates_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("web.log");
        let mut spec = sh("i=0; while [ $i -lt 50 ]; do echo line-$i; i=$((i+1)); done");
        spec.log_path = Some(log.clone());
        spec.log_rotate_bytes = 200;
        let p = spawn(spec);
        all_output(&p.0).await;

        let rotated = fs::read_to_string(crate::rotated_log_path(&log)).unwrap();
        let current = fs::read_to_string(&log).unwrap();
        assert!(rotated.contains("line-"), "{rotated}");
        assert!(current.contains("line-49"), "{current}");
        assert!(!current.contains("line-0\r\n"), "{current}");
    }

    #[test]
    fn unwritable_log_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, "").unwrap();
        let mut spec = sh("true");
        spec.log_path = Some(blocker.join("web.log"));
        assert!(matches!(
            ManagedProcess::spawn(spec),
            Err(ProcError::Log { .. })
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn records_start_time() {
        let p = spawn(sh("sleep 5"));
        let record = p.0.pid_record().unwrap();
        assert_eq!(record.pid, p.0.pid());
        assert_eq!(Some(record.start_time), process_start_time(p.0.pid()));
        p.0.terminate(Duration::from_secs(2)).await;
    }
}
