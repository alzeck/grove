use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where a log goes when it is rotated: `<file>.1`.
pub fn rotated_log_path(path: &Path) -> PathBuf {
    let mut rotated = OsString::from(path.as_os_str());
    rotated.push(".1");
    rotated.into()
}

/// An append-only log that moves itself to `<file>.1` before growing past
/// `rotate_at` bytes. Writes are unbuffered.
pub(crate) struct LogFile {
    path: PathBuf,
    file: File,
    len: u64,
    rotate_at: u64,
}

impl LogFile {
    /// Opens (creating parent directories) and writes a "started" header.
    pub fn open(path: &Path, rotate_at: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_append(path)?;
        let len = file.metadata()?.len();
        let mut log = Self {
            path: path.to_owned(),
            file,
            len,
            rotate_at,
        };
        log.write(format!("\r\n── started {} ──\r\n", local_timestamp()).as_bytes())?;
        Ok(log)
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.len > 0 && self.len + data.len() as u64 > self.rotate_at {
            self.rotate()?;
        }
        self.file.write_all(data)?;
        self.len += data.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        fs::rename(&self.path, rotated_log_path(&self.path))?;
        self.file = open_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// `2026-09-26 14:03:07` in local time.
fn local_timestamp() -> String {
    // SAFETY: `localtime_r` and `strftime` only write into the buffers we
    // pass; the format string is NUL-terminated.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return now.to_string();
        }
        let mut buf = [0u8; 64];
        let n = libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y-%m-%d %H:%M:%S".as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_header_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/web.log");
        let mut log = LogFile::open(&path, 1 << 20).unwrap();
        log.write(b"hello\r\n").unwrap();
        drop(log);
        LogFile::open(&path, 1 << 20).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("── started ").count(), 2);
        assert!(text.contains("hello\r\n"));
        assert!(text.starts_with("\r\n── started 20"));
    }

    #[test]
    fn rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.log");
        let mut log = LogFile::open(&path, 100).unwrap();
        log.write(&[b'a'; 50]).unwrap();
        log.write(&[b'b'; 60]).unwrap();

        let rotated = fs::read_to_string(rotated_log_path(&path)).unwrap();
        assert!(rotated.contains("started"));
        assert!(rotated.ends_with(&"a".repeat(50)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "b".repeat(60));

        // A chunk bigger than the limit still gets written, to a fresh file.
        log.write(&[b'c'; 150]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "c".repeat(150));
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path)).unwrap(),
            "b".repeat(60)
        );
    }

    #[test]
    fn rotates_an_oversized_file_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.log");
        fs::write(&path, "x".repeat(200)).unwrap();
        LogFile::open(&path, 100).unwrap();
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path)).unwrap(),
            "x".repeat(200)
        );
        assert!(fs::read_to_string(&path).unwrap().contains("started"));
    }
}
