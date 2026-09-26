use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::net::TcpSocket;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const RECHECK_AFTER: Duration = Duration::from_millis(50);
const LSOF_TIMEOUT: Duration = Duration::from_secs(3);

/// True if nothing listens on `port`: it can be bound on 127.0.0.1 and
/// 0.0.0.0 (and ::1 and :: when IPv6 is available), and nothing accepts a
/// connection on 127.0.0.1. Binds use SO_REUSEADDR like dev servers do, so
/// sockets in TIME_WAIT don't count as taken. Blocks for up to ~50ms when
/// the port is busy.
pub fn port_is_free(port: u16) -> bool {
    // A child forked by another thread holds copies of our probe sockets
    // until it execs, which can make a free port look busy for a moment.
    // That only ever errs towards "busy", so one clean check is enough.
    port != 0
        && (check_port(port) || {
            std::thread::sleep(RECHECK_AFTER);
            check_port(port)
        })
}

fn check_port(port: u16) -> bool {
    let v4 = [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED];
    if v4.iter().any(|ip| bind((*ip, port).into()).is_err()) {
        return false;
    }
    for ip in [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED] {
        match bind((ip, port).into()) {
            Ok(()) => {}
            Err(err) if ipv6_unavailable(&err) => break,
            Err(_) => return false,
        }
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_err()
}

/// Binds without listening, so a copy leaked into a forked child can't
/// accept (and then reset) anyone's connection.
fn bind(addr: SocketAddr) -> io::Result<()> {
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)
}

fn ipv6_unavailable(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::AddrNotAvailable || err.raw_os_error() == Some(libc::EAFNOSUPPORT)
}

/// Who is listening on `port`, e.g. `"node (pid 123)"`. Best effort, via
/// `lsof`; blocks for up to a few seconds.
pub fn port_owner(port: u16) -> Option<String> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.exists())
        .unwrap_or(Path::new("lsof"));
    let mut child = Command::new(lsof)
        .args([
            "-nP",
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "+c",
            "0",
            "-Fpc",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // The output for one port is tiny, so lsof can't block on a full pipe.
    let deadline = Instant::now() + LSOF_TIMEOUT;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut out = String::new();
    io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_lsof(&out)
}

/// Parses `lsof -F pc` output: `p<pid>` lines, each followed by `c<command>`.
fn parse_lsof(out: &str) -> Option<String> {
    let mut pid = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = Some(p);
        } else if let (Some(command), Some(pid)) = (line.strip_prefix('c'), pid) {
            return Some(format!("{command} (pid {pid})"));
        }
    }
    pid.map(|pid| format!("pid {pid}"))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// A port below the ephemeral range (49152+ on macOS): a freed ephemeral
    /// port can immediately become the source port of some other test's
    /// outgoing connection, which rightly makes it not free.
    fn free_port() -> u16 {
        (41000..42000)
            .find(|&p| bind((Ipv4Addr::LOCALHOST, p).into()).is_ok())
            .expect("a free port")
    }

    #[test]
    fn free_port_is_free() {
        assert!(port_is_free(free_port()));
        assert!(!port_is_free(0));
    }

    #[test]
    fn bound_port_is_not_free() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        drop(listener);

        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        assert!(!port_is_free(listener.local_addr().unwrap().port()));

        if let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
            assert!(!port_is_free(listener.local_addr().unwrap().port()));
        }
    }

    #[test]
    fn parses_lsof() {
        assert_eq!(
            parse_lsof("p123\ncnode\np456\ncruby\n").as_deref(),
            Some("node (pid 123)")
        );
        assert_eq!(parse_lsof("p9\n").as_deref(), Some("pid 9"));
        assert_eq!(parse_lsof(""), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finds_the_owner() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let owner = port_owner(port).expect("lsof should see our listener");
        assert!(
            owner.ends_with(&format!("(pid {})", std::process::id())),
            "{owner}"
        );
        drop(listener);
        assert_eq!(port_owner(port), None);
    }
}
