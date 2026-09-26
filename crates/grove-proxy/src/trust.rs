//! Trusting the CA in the macOS login keychain via `security`.

use crate::ca::CertAuthority;
use crate::command;
use crate::error::CaError;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

const SECURITY: &str = "/usr/bin/security";

impl CertAuthority {
    /// Whether macOS trusts the CA for TLS servers. Read-only and never
    /// prompts.
    pub fn is_trusted(&self) -> bool {
        // Evaluates the root itself against the SSL policy; succeeds only if
        // user or admin trust settings make it an anchor. `-L` keeps it offline.
        let result = command::run(
            Command::new(SECURITY)
                .args(["verify-cert", "-q", "-L", "-p", "ssl", "-c"])
                .arg(self.cert_path()),
            Duration::from_secs(10),
        );
        matches!(result, Ok(out) if out.status.success())
    }

    /// Adds the CA to the login keychain as a trusted root. macOS asks for
    /// the user's password, so this blocks until they answer.
    pub fn install_trust(&self) -> Result<(), CaError> {
        let keychain = login_keychain().ok_or_else(|| CaError::Security {
            command: "add-trusted-cert",
            message: "HOME is not set".into(),
        })?;
        let out = Command::new(SECURITY)
            .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
            .arg(keychain)
            .arg(self.cert_path())
            .output();
        check("add-trusted-cert", out)
    }

    /// Removes the trust settings added by [`install_trust`](Self::install_trust).
    /// Also prompts. The certificate itself stays in the keychain, untrusted.
    pub fn uninstall_trust(&self) -> Result<(), CaError> {
        let out = Command::new(SECURITY)
            .arg("remove-trusted-cert")
            .arg(self.cert_path())
            .output();
        match check("remove-trusted-cert", out) {
            Err(_) if !self.is_trusted() => Ok(()),
            result => result,
        }
    }
}

fn login_keychain() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
}

fn check(command: &'static str, out: std::io::Result<Output>) -> Result<(), CaError> {
    let message = match out {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            if stderr.is_empty() {
                out.status.to_string()
            } else {
                stderr
            }
        }
        Err(e) => e.to_string(),
    };
    Err(CaError::Security { command, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ca_is_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = CertAuthority::load_or_create(tmp.path()).unwrap();
        assert!(!ca.is_trusted());
    }
}
