//! Running the `git` binary. Output is made stable (`LC_ALL=C`) and git never
//! prompts (`GIT_TERMINAL_PROMPT=0`, stdin closed). The user's own git config,
//! credential helpers and hooks are respected.

use crate::error::{GitError, Result};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Output, Stdio};
use tokio::process::Command;

/// A `git -C <dir> …` invocation.
pub(crate) struct Git {
    args: Vec<OsString>,
    envs: Vec<(&'static str, &'static str)>,
}

pub(crate) fn git(dir: &Path) -> Git {
    Git {
        args: vec!["-C".into(), dir.into()],
        envs: Vec::new(),
    }
}

impl Git {
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, key: &'static str, value: &'static str) -> Self {
        self.envs.push((key, value));
        self
    }

    /// Runs git and returns its raw output, whatever the exit code.
    pub async fn output(&self) -> Result<Output> {
        tracing::debug!(command = %self.describe(), "running git");
        let mut cmd = Command::new("git");
        cmd.args(&self.args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .envs(self.envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(test)]
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.output().await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => GitError::NotInstalled,
            _ => GitError::Io(e),
        })
    }

    /// Runs git and returns stdout, or a [`GitError::Command`] on failure.
    pub async fn run(&self) -> Result<String> {
        let out = self.output().await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(self.failure(&out))
        }
    }

    /// For commands that answer a yes/no question through their exit code:
    /// 0 means yes, 1 means no, anything else is an error.
    pub async fn probe(&self) -> Result<bool> {
        let out = self.output().await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.failure(&out)),
        }
    }

    pub fn failure(&self, out: &Output) -> GitError {
        GitError::Command {
            command: self.describe(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code(),
        }
    }

    fn describe(&self) -> String {
        let mut s = String::from("git");
        for arg in &self.args {
            let arg = arg.to_string_lossy();
            s.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                s.push_str(&format!("'{arg}'"));
            } else {
                s.push_str(&arg);
            }
        }
        s
    }
}

/// The installed git version, e.g. `2.55.0` (for diagnostics).
pub async fn git_version() -> Result<String> {
    let out = git(Path::new(".")).arg("--version").run().await?;
    // "git version 2.55.0" or "git version 2.39.5 (Apple Git-154)"
    Ok(out
        .split_whitespace()
        .nth(2)
        .unwrap_or(out.trim())
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_quotes_whitespace() {
        let g = git(Path::new("/tmp/my repo")).args(["commit", "-m", "a b"]);
        assert_eq!(g.describe(), "git -C '/tmp/my repo' commit -m 'a b'");
    }

    #[tokio::test]
    async fn version() {
        let v = git_version().await.unwrap();
        assert!(v.starts_with(|c: char| c.is_ascii_digit()), "{v}");
    }
}
