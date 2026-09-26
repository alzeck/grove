//! Thin wrappers over the libc process calls.

use std::io;

use crate::exit::ExitInfo;

/// Converts to a `pid_t`, refusing 0 and 1: `killpg(0)` would hit our own
/// group and pid 1 is launchd.
fn to_pid(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|p| *p > 1)
}

fn last_errno() -> Option<i32> {
    io::Error::last_os_error().raw_os_error()
}

/// Returns false if the group doesn't exist (or can't be signalled).
pub(crate) fn signal_group(pgid: u32, sig: libc::c_int) -> bool {
    // SAFETY: plain syscall; `to_pid` rules out our own group and launchd.
    to_pid(pgid).is_some_and(|pgid| unsafe { libc::killpg(pgid, sig) } == 0)
}

pub(crate) fn signal_pid(pid: u32, sig: libc::c_int) -> bool {
    // SAFETY: plain syscall; `to_pid` rules out pid 0 and 1.
    to_pid(pid).is_some_and(|pid| unsafe { libc::kill(pid, sig) } == 0)
}

/// True while any process, zombies included, is in the group.
pub(crate) fn group_exists(pgid: u32) -> bool {
    let Some(pgid) = to_pid(pgid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::killpg(pgid, 0) == 0 || last_errno() == Some(libc::EPERM) }
}

/// True if the process exists and is not a zombie.
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Some(p) = to_pid(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence.
    let exists = unsafe { libc::kill(p, 0) == 0 || last_errno() == Some(libc::EPERM) };
    exists && !is_zombie(pid)
}

pub(crate) fn is_group_leader(pid: u32) -> bool {
    // SAFETY: plain syscall.
    to_pid(pid).is_some_and(|p| unsafe { libc::getpgid(p) } == p)
}

#[cfg(target_os = "macos")]
pub(crate) fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: the buffer is a zeroed `proc_bsdinfo` of exactly `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: a full-size result means the kernel filled the struct (and it
    // was zeroed to begin with, which is a valid value for it).
    (n == size).then(|| unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
    bsd_info(pid).is_some_and(|info| info.pbi_status == libc::SZOMB)
}

#[cfg(not(target_os = "macos"))]
fn is_zombie(_pid: u32) -> bool {
    false
}

/// Blocks until our child `pid` exits and reaps it.
pub(crate) fn wait_pid(pid: u32) -> ExitInfo {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return ExitInfo::default();
    };
    let mut status = 0;
    loop {
        // SAFETY: `status` is a valid out pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            return ExitInfo::from_wait_status(status);
        }
        if rc == -1 && last_errno() == Some(libc::EINTR) {
            continue;
        }
        // ECHILD: someone else reaped it, so the status is unknown.
        return ExitInfo::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_dangerous_pids() {
        assert!(!signal_group(0, 0));
        assert!(!signal_group(1, 0));
        assert!(!signal_pid(0, 0));
        assert!(!group_exists(0));
        assert!(!pid_alive(1));
    }

    #[test]
    fn sees_itself() {
        assert!(pid_alive(std::process::id()));
    }
}
