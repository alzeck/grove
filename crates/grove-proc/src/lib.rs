//! Spawning and supervising dev processes: PTYs, process groups, output
//! replay and log files, readiness checks, shell environment capture, port
//! checks, and reaping orphans left behind by a crash.

mod error;
mod exit;
mod log;
mod orphans;
mod ports;
mod process;
mod ready;
mod replay;
mod shell_env;
mod sys;

pub use error::ProcError;
pub use exit::{ExitInfo, ProcStatus, signal_name};
pub use log::rotated_log_path;
pub use orphans::{PidRecord, process_start_time, reap_orphans};
pub use ports::{port_is_free, port_owner};
pub use process::{
    DEFAULT_LOG_ROTATE_BYTES, DEFAULT_REPLAY_BYTES, ManagedProcess, OutputSubscription, SpawnSpec,
};
pub use ready::{ReadyError, wait_ready};
pub use shell_env::{capture_shell_env, capture_shell_env_with, user_shell};
