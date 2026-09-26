//! Running the `gh` binary non-interactively.

use crate::error::GhError;
use std::process::Stdio;
use tokio::process::Command;

/// gh's exit code when a command needs authentication.
const EXIT_AUTH: i32 = 4;

/// Runs `gh <args>` and returns stdout.
pub(crate) async fn gh(args: &[&str]) -> Result<String, GhError> {
    let out = Command::new("gh")
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => GhError::NotInstalled,
            _ => GhError::Command {
                args: to_strings(args),
                stderr: e.to_string(),
            },
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(match out.status.code() {
        Some(EXIT_AUTH) => GhError::NotAuthenticated(stderr),
        _ => GhError::Command {
            args: to_strings(args),
            stderr,
        },
    })
}

fn to_strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// The installed gh version, e.g. `2.97.0`.
pub async fn gh_version() -> Result<String, GhError> {
    parse_version(&gh(&["--version"]).await?)
}

fn parse_version(out: &str) -> Result<String, GhError> {
    // "gh version 2.97.0 (2026-07-31)\nhttps://github.com/cli/cli/releases/…"
    out.lines()
        .next()
        .and_then(|line| line.strip_prefix("gh version "))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| GhError::Parse(format!("gh --version printed {:?}", out.trim())))
}

/// Succeeds when gh is logged in to github.com.
pub async fn auth_status() -> Result<(), GhError> {
    match gh(&["auth", "status", "--hostname", "github.com"]).await {
        Ok(_) => Ok(()),
        Err(GhError::Command { stderr, .. }) => Err(GhError::NotAuthenticated(stderr)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        let out =
            "gh version 2.97.0 (2026-07-31)\nhttps://github.com/cli/cli/releases/tag/v2.97.0\n";
        assert_eq!(parse_version(out).unwrap(), "2.97.0");
        assert_eq!(
            parse_version("gh version 2.40.1-pre\n").unwrap(),
            "2.40.1-pre"
        );
        assert!(matches!(parse_version("hub 1.0"), Err(GhError::Parse(_))));
    }

    #[test]
    fn error_display() {
        let e = GhError::Command {
            args: to_strings(&["pr", "view", "1"]),
            stderr: "GraphQL: Could not resolve to a PullRequest\n".into(),
        };
        assert_eq!(
            e.to_string(),
            "`gh pr view 1` failed: GraphQL: Could not resolve to a PullRequest"
        );
    }
}
