//! The local root CA and on-demand leaf certificates.

use crate::error::CaError;
use parking_lot::Mutex;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

const CERT_FILE: &str = "ca.pem";
const KEY_FILE: &str = "ca-key.pem";
const CA_NAME: &str = "Grove Local CA";
const CA_VALIDITY: Duration = Duration::days(3650);
/// Apple rejects TLS leaves valid for more than 398 days.
const LEAF_VALIDITY: Duration = Duration::days(397);
/// Tolerates clocks that are slightly behind.
const BACKDATE: Duration = Duration::days(1);

pub struct CertAuthority {
    dir: PathBuf,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
    provider: Arc<CryptoProvider>,
    leaves: Mutex<HashMap<String, Leaf>>,
}

struct Leaf {
    key: Arc<CertifiedKey>,
    renew_after: OffsetDateTime,
}

impl CertAuthority {
    /// Loads `ca.pem` and `ca-key.pem` from `dir`, generating them on first
    /// run.
    pub fn load_or_create(dir: &Path) -> Result<Self, CaError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Self::load(dir, &cert_path, &key_path),
            (false, false) => Self::create(dir, &cert_path, &key_path),
            _ => {
                tracing::warn!(dir = %dir.display(), "incomplete CA files, generating a new CA");
                Self::create(dir, &cert_path, &key_path)
            }
        }
    }

    /// `ca.pem`, e.g. for `NODE_EXTRA_CA_CERTS`.
    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CERT_FILE)
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// A leaf certificate for exactly `host`, chained to the root. Cached per
    /// host.
    pub fn issue(&self, host: &str) -> Result<Arc<CertifiedKey>, CaError> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let now = OffsetDateTime::now_utc();
        let mut leaves = self.leaves.lock();
        if let Some(leaf) = leaves.get(&host)
            && now < leaf.renew_after
        {
            return Ok(leaf.key.clone());
        }
        let leaf = self.mint(&host, now)?;
        let key = leaf.key.clone();
        leaves.insert(host, leaf);
        Ok(key)
    }

    pub(crate) fn provider(&self) -> Arc<CryptoProvider> {
        self.provider.clone()
    }

    fn create(dir: &Path, cert_path: &Path, key_path: &Path) -> Result<Self, CaError> {
        if !dir.exists() {
            fs::create_dir_all(dir).map_err(|e| CaError::io(dir, e))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| CaError::io(dir, e))?;
        }
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = ca_params(OffsetDateTime::now_utc()).self_signed(&key)?;
        let cert_pem = cert.pem();
        // Key first: a cert without its key would be useless.
        write_file(key_path, &key.serialize_pem(), 0o600)?;
        write_file(cert_path, &cert_pem, 0o644)?;
        Self::from_parts(dir, cert_path, cert_pem, key)
    }

    fn load(dir: &Path, cert_path: &Path, key_path: &Path) -> Result<Self, CaError> {
        let cert_pem = fs::read_to_string(cert_path).map_err(|e| CaError::io(cert_path, e))?;
        let key_pem = fs::read_to_string(key_path).map_err(|e| CaError::io(key_path, e))?;
        let mode = fs::metadata(key_path)
            .map_err(|e| CaError::io(key_path, e))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))
                .map_err(|e| CaError::io(key_path, e))?;
        }
        let key = KeyPair::from_pem(&key_pem).map_err(|e| CaError::Invalid {
            path: key_path.to_owned(),
            message: e.to_string(),
        })?;
        Self::from_parts(dir, cert_path, cert_pem, key)
    }

    fn from_parts(
        dir: &Path,
        cert_path: &Path,
        cert_pem: String,
        key: KeyPair,
    ) -> Result<Self, CaError> {
        let invalid = |message: String| CaError::Invalid {
            path: cert_path.to_owned(),
            message,
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| invalid(e.to_string()))?;
        // Fails if the key doesn't belong to the certificate.
        CertifiedKey::from_der(vec![cert_der.clone()], private_key(&key), &provider)
            .map_err(|e| invalid(format!("doesn't match {KEY_FILE}: {e}")))?;
        let issuer =
            Issuer::from_ca_cert_der(&cert_der, key).map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            dir: dir.to_owned(),
            cert_pem,
            cert_der,
            issuer,
            provider,
            leaves: Mutex::default(),
        })
    }

    fn mint(&self, host: &str, now: OffsetDateTime) -> Result<Leaf, CaError> {
        let mut params = CertificateParams::new(vec![host.to_owned()])?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host);
        params.not_before = now - BACKDATE;
        params.not_after = params.not_before + LEAF_VALIDITY;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;

        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = params.signed_by(&key, &self.issuer)?;
        let signing_key = self
            .provider
            .key_provider
            .load_private_key(private_key(&key))?;
        Ok(Leaf {
            key: Arc::new(CertifiedKey::new(
                vec![cert.der().clone(), self.cert_der.clone()],
                signing_key,
            )),
            renew_after: params.not_after - Duration::days(1),
        })
    }
}

impl fmt::Debug for CertAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertAuthority")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn ca_params(now: OffsetDateTime) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = now - BACKDATE;
    params.not_after = now + CA_VALIDITY;
    params
}

fn private_key(key: &KeyPair) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(key.serialize_der().into())
}

/// Writes via a temp file and rename, so a crash never leaves a truncated
/// file behind.
fn write_file(path: &Path, contents: &str, mode: u32) -> Result<(), CaError> {
    let tmp = path.with_extension("pem.tmp");
    let write = || -> io::Result<()> {
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| CaError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::RootCertStore;
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    fn verify(ca_pem: &str, key: &CertifiedKey, name: &str) -> Result<(), rustls::Error> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap())
            .unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                &key.cert[0],
                &key.cert[1..],
                &ServerName::try_from(name.to_owned()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn creates_then_loads_the_same_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ca");
        let created = CertAuthority::load_or_create(&dir).unwrap();
        assert_eq!(created.cert_path(), dir.join("ca.pem"));
        assert_eq!(
            fs::read_to_string(created.cert_path()).unwrap(),
            created.cert_pem()
        );
        let key_mode = fs::metadata(dir.join("ca-key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(key_mode & 0o777, 0o600);
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let loaded = CertAuthority::load_or_create(&dir).unwrap();
        assert_eq!(loaded.cert_pem(), created.cert_pem());

        // Leaves from the reloaded CA chain to the original root.
        let leaf = loaded.issue("api.localhost").unwrap();
        verify(created.cert_pem(), &leaf, "api.localhost").unwrap();
    }

    #[test]
    fn issued_leaf_verifies_for_its_host_only() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = CertAuthority::load_or_create(tmp.path()).unwrap();
        let leaf = ca.issue("tenant.api.localhost").unwrap();
        assert_eq!(leaf.cert.len(), 2);
        leaf.keys_match().unwrap();
        verify(ca.cert_pem(), &leaf, "tenant.api.localhost").unwrap();
        assert!(verify(ca.cert_pem(), &leaf, "other.api.localhost").is_err());

        let other = tempfile::tempdir().unwrap();
        let stranger = CertAuthority::load_or_create(other.path()).unwrap();
        assert!(verify(stranger.cert_pem(), &leaf, "tenant.api.localhost").is_err());
    }

    #[test]
    fn leaves_are_cached_per_host() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = CertAuthority::load_or_create(tmp.path()).unwrap();
        let a = ca.issue("api.localhost").unwrap();
        assert!(Arc::ptr_eq(&a, &ca.issue("API.localhost.").unwrap()));
        assert!(!Arc::ptr_eq(&a, &ca.issue("web.localhost").unwrap()));
    }

    #[test]
    fn loosened_key_permissions_are_restored() {
        let tmp = tempfile::tempdir().unwrap();
        CertAuthority::load_or_create(tmp.path()).unwrap();
        let key = tmp.path().join("ca-key.pem");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        CertAuthority::load_or_create(tmp.path()).unwrap();
        let mode = fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        CertAuthority::load_or_create(a.path()).unwrap();
        CertAuthority::load_or_create(b.path()).unwrap();
        fs::copy(b.path().join("ca-key.pem"), a.path().join("ca-key.pem")).unwrap();
        let err = CertAuthority::load_or_create(a.path()).unwrap_err();
        assert!(matches!(err, CaError::Invalid { .. }), "{err}");
    }
}
