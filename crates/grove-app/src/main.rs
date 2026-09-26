//! `grove`: the desktop app when run without arguments, a CLI that drives
//! the running app otherwise.

mod cli;
mod gui;
mod headless;
mod launch;
mod shell_path;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "Run several copies of your dev environment side by side"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Start the app without showing its window (used by the CLI).
    #[arg(long, hide = true)]
    background: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run Grove without the GUI; Ctrl-C stops every process.
    Headless {
        /// Don't start the HTTPS proxy.
        #[arg(long)]
        no_proxy: bool,
        #[arg(long, env = "GROVE_HTTPS_PORT", default_value_t = 443)]
        https_port: u16,
        #[arg(long, env = "GROVE_HTTP_PORT", default_value_t = 80)]
        http_port: u16,
    },
    /// List clusters.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Show a cluster's projects, processes and URLs.
    Status {
        cluster: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create a cluster from a pull request or a branch.
    New(NewArgs),
    /// Start a cluster, or one `project/process`.
    Up {
        cluster: String,
        target: Option<String>,
    },
    /// Stop a cluster, or one `project/process`.
    Down {
        cluster: String,
        target: Option<String>,
    },
    /// Restart a cluster, or one `project/process`.
    Restart {
        cluster: String,
        target: Option<String>,
    },
    /// Tear down a cluster: stop it, remove its worktrees and fresh databases.
    Rm {
        cluster: String,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        /// Remove worktrees even if they have uncommitted changes.
        #[arg(long)]
        force: bool,
        /// Also remove worktrees Grove adopted rather than created.
        #[arg(long)]
        remove_adopted: bool,
    },
    /// Resume creating a cluster whose creation failed.
    Retry {
        cluster: String,
        #[arg(long)]
        no_start: bool,
    },
    /// `git pull --ff-only` a project in a cluster, then run its after_pull hooks.
    Pull { cluster: String, project: String },
    /// Show a process's output (`project/process`).
    Logs {
        cluster: String,
        target: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Open a cluster's URL in the browser.
    Open {
        cluster: String,
        /// `project` or `project/process`; defaults to the first URL.
        target: Option<String>,
    },
    /// Open a project's checkout in your editor.
    Edit { cluster: String, project: String },
    /// List worktrees that no cluster uses (e.g. made by other tools).
    Worktrees,
    /// Check a workspace for config problems.
    Validate { path: Option<PathBuf> },
    /// Check git, gh, Postgres, the proxy and the certificate.
    Doctor,
    /// Re-read config files.
    Reload,
}

#[derive(Args)]
struct NewArgs {
    /// PR URL, number, `#123` or `owner/repo#123`.
    #[arg(long, conflicts_with = "branch", required_unless_present = "branch")]
    pr: Option<String>,
    /// Branch name; projects that have it get a worktree.
    #[arg(long)]
    branch: Option<String>,
    /// Base for creating `--branch` in `--project` when it doesn't exist.
    #[arg(long, requires = "branch")]
    base: Option<String>,
    /// Project the PR number or new branch belongs to.
    #[arg(long)]
    project: Option<String>,
    /// Cluster name (default: pr-<n> or the branch).
    #[arg(long)]
    name: Option<String>,
    /// Give these projects a fresh database.
    #[arg(long = "fresh-db", value_name = "PROJECT")]
    fresh_db: Vec<String>,
    /// Use the shared database for these projects.
    #[arg(long = "shared-db", value_name = "PROJECT")]
    shared_db: Vec<String>,
    /// Reuse the default cluster's instance for these projects.
    #[arg(long, value_name = "PROJECT")]
    reuse: Vec<String>,
    /// Create without starting.
    #[arg(long)]
    no_start: bool,
    /// Don't ask for confirmation.
    #[arg(short, long)]
    yes: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            // Launched from Finder: pick up the login shell's PATH (brew, git, gh…)
            // before any threads exist.
            shell_path::adopt_login_shell_path();
            gui::run(cli.background)
        }
        Some(Command::Headless {
            no_proxy,
            https_port,
            http_port,
        }) => {
            shell_path::adopt_login_shell_path();
            headless::run(no_proxy, https_port, http_port)
        }
        Some(command) => cli::run(command),
    }
}
