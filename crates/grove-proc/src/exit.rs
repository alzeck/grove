use std::fmt;

use serde::{Deserialize, Serialize};

/// How a process ended. `signal` is set when it was killed by a signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == Some(0)
    }

    pub(crate) fn from_wait_status(status: libc::c_int) -> Self {
        if libc::WIFEXITED(status) {
            Self {
                code: Some(libc::WEXITSTATUS(status)),
                signal: None,
            }
        } else if libc::WIFSIGNALED(status) {
            Self {
                code: None,
                signal: Some(libc::WTERMSIG(status)),
            }
        } else {
            Self::default()
        }
    }
}

impl fmt::Display for ExitInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.code, self.signal) {
            (_, Some(sig)) => match signal_name(sig) {
                Some(name) => write!(f, "killed by {name}"),
                None => write!(f, "killed by signal {sig}"),
            },
            (Some(code), None) => write!(f, "exit code {code}"),
            (None, None) => f.write_str("exited"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcStatus {
    Running,
    Exited(ExitInfo),
}

/// `SIGTERM`-style name for the common signals.
pub fn signal_name(sig: i32) -> Option<&'static str> {
    Some(match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        let failed = ExitInfo {
            code: Some(1),
            signal: None,
        };
        assert_eq!(failed.to_string(), "exit code 1");
        assert!(!failed.success());

        let killed = ExitInfo {
            code: None,
            signal: Some(libc::SIGTERM),
        };
        assert_eq!(killed.to_string(), "killed by SIGTERM");
        assert!(!killed.success());

        let odd = ExitInfo {
            code: None,
            signal: Some(63),
        };
        assert_eq!(odd.to_string(), "killed by signal 63");

        assert!(
            ExitInfo {
                code: Some(0),
                signal: None
            }
            .success()
        );
    }
}
