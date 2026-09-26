use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::sys;

const POLL: Duration = Duration::from_millis(50);

/// A process group Grove started, identified robustly enough to survive PID
/// reuse: `pid` is the group leader, `start_time` from [`process_start_time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub start_time: u64,
}

/// When `pid` started, in seconds since the epoch. Always `None` off macOS.
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    sys::bsd_info(pid).map(|info| info.pbi_start_tvsec)
}

#[cfg(not(target_os = "macos"))]
pub fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Kills leftovers from a previous run. A record only counts if its pid is
/// alive and started at the recorded time, so a reused pid is never
/// signalled. Its process group gets SIGTERM, then SIGKILL after `grace`.
/// Returns how many records were reaped.
pub async fn reap_orphans(records: &[PidRecord], grace: Duration) -> usize {
    let targets: Vec<Target> = records
        .iter()
        .filter(|r| sys::pid_alive(r.pid) && process_start_time(r.pid) == Some(r.start_time))
        .map(|r| Target {
            pid: r.pid,
            group: sys::is_group_leader(r.pid),
        })
        .collect();
    for target in &targets {
        target.signal(libc::SIGTERM);
    }

    let deadline = Instant::now() + grace;
    loop {
        let alive: Vec<&Target> = targets.iter().filter(|t| t.alive()).collect();
        if alive.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for target in alive {
                target.signal(libc::SIGKILL);
            }
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    targets.len()
}

struct Target {
    pid: u32,
    /// Leader of its own group (always true for processes Grove spawned).
    group: bool,
}

impl Target {
    fn signal(&self, sig: libc::c_int) {
        if self.group {
            sys::signal_group(self.pid, sig);
        } else {
            sys::signal_pid(self.pid, sig);
        }
    }

    fn alive(&self) -> bool {
        if self.group {
            sys::group_exists(self.pid)
        } else {
            sys::pid_alive(self.pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn start_time_of_self() {
        let start = process_start_time(std::process::id()).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(start <= now && now - start < 3600, "{start} vs {now}");
        assert_eq!(process_start_time(u32::MAX), None);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reaps_matching_records_only() {
        let mut child = Command::new("sleep")
            .arg("100")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap();

        // A different start time means the pid was reused: leave it alone.
        let stale = PidRecord {
            pid,
            start_time: start_time + 1,
        };
        assert_eq!(reap_orphans(&[stale], Duration::from_secs(1)).await, 0);
        assert!(sys::pid_alive(pid));

        // We're the parent here (launchd would be after a crash), so reap it.
        let waiter = std::thread::spawn(move || child.wait().unwrap());
        let record = PidRecord { pid, start_time };
        assert_eq!(reap_orphans(&[record], Duration::from_secs(5)).await, 1);
        let status = waiter.join().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));

        assert_eq!(reap_orphans(&[record], Duration::from_secs(1)).await, 0);
    }

    #[tokio::test]
    async fn ignores_dead_and_bogus_records() {
        let records = [
            PidRecord {
                pid: 0,
                start_time: 0,
            },
            PidRecord {
                pid: 1,
                start_time: 0,
            },
            PidRecord {
                pid: u32::MAX,
                start_time: 0,
            },
        ];
        assert_eq!(reap_orphans(&records, Duration::from_millis(10)).await, 0);
    }
}
