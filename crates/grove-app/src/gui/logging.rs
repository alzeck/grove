//! Logs go to `~/.grove/logs/grove.log` and stderr.

use grove_config::GroveHome;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

pub fn init(home: &GroveHome) {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

    let filter = EnvFilter::try_from_env("GROVE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let path = home.logs_dir().join("grove.log");
    let _ = std::fs::create_dir_all(home.logs_dir());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    // The CLI starts the app with stderr already pointing at grove.log; writing
    // to both would log every line twice.
    let file_layer = file
        .filter(|_| !stderr_is(&path))
        .map(|f| fmt::layer().with_ansi(false).with_writer(Mutex::new(f)));
    let stderr_layer = fmt::layer()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
}

/// Whether stderr is `path` (same device and inode).
fn stderr_is(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    // SAFETY: fstat only writes into the zeroed struct we pass.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(libc::STDERR_FILENO, &mut st) } != 0 {
        return false;
    }
    st.st_dev as u64 == meta.dev() && st.st_ino == meta.ino()
}
