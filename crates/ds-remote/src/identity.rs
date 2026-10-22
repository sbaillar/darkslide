//! A device's long-lived identity: an Ed25519 key and a self-signed
//! certificate for it, made once and kept in the state directory
//! (`identity.key` is owner-only).
//!
//! Peers pin the [`Fingerprint`] of the *key* (BLAKE3 of the 32-byte public
//! key), not of the certificate, so the same key can be reissued in a new
//! certificate without re-pairing.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::{RemoteError, Result};

const KEY_FILE: &str = "identity.key";
const CERT_FILE: &str = "identity.crt";

/// DER of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410) up to the key: the
/// whole SPKI is these 12 bytes and the 32-byte key.
const ED25519_SPKI: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// BLAKE3 of a device's Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_key(public_key: &[u8; 32]) -> Self {
        Self(*blake3::hash(public_key).as_bytes())
    }

    /// The fingerprint of a certificate's subject key (the X.509 is parsed
    /// with webpki); `None` if it does not parse or the key is not Ed25519.
    pub fn of_cert(cert_der: &[u8]) -> Option<Self> {
        let der = rustls::pki_types::CertificateDer::from(cert_der);
        let cert = webpki::EndEntityCert::try_from(&der).ok()?;
        let spki = cert.subject_public_key_info();
        let key = spki.as_ref().strip_prefix(&ED25519_SPKI[..])?;
        Some(Self::of_key(key.try_into().ok()?))
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        blake3::Hash::from_hex(s).ok().map(|h| Self(*h.as_bytes()))
    }

    /// Short form for people to compare by eye: `ab12 cd34 ef56 7890`.
    pub fn short(&self) -> String {
        let hex = self.to_hex();
        hex.as_bytes()[..16]
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("hex is ASCII"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A device key and its certificate.
#[derive(Clone)]
pub struct Identity {
    key_pkcs8: Vec<u8>,
    cert_der: Vec<u8>,
    fingerprint: Fingerprint,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A fresh key and certificate (not stored).
    pub fn generate() -> Result<Self> {
        let err = |e: rcgen::Error| RemoteError::Identity(e.to_string());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(err)?;
        let mut params =
            rcgen::CertificateParams::new(vec!["darkslide".to_owned()]).map_err(err)?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Darkslide device");
        let cert = params.self_signed(&key).map_err(err)?;
        Self::from_parts(key.serialize_der(), cert.der().to_vec())
    }

    fn from_parts(key_pkcs8: Vec<u8>, cert_der: Vec<u8>) -> Result<Self> {
        let fingerprint = Fingerprint::of_cert(&cert_der)
            .ok_or_else(|| RemoteError::Identity("not an Ed25519 certificate".into()))?;
        Ok(Self {
            key_pkcs8,
            cert_der,
            fingerprint,
        })
    }

    /// The identity stored in `dir`, made and stored on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (kp, cp) = (dir.join(KEY_FILE), dir.join(CERT_FILE));
        match (fs::read(&kp), fs::read(&cp)) {
            (Ok(key), Ok(cert)) => Self::from_parts(key, cert),
            (Err(e), _) | (_, Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(RemoteError::file(dir, e))
            }
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => Err(RemoteError::Identity(format!(
                "{} holds half an identity; move it aside to make a new one",
                dir.display()
            ))),
            (Err(_), Err(_)) => {
                let id = Self::generate()?;
                fs::create_dir_all(dir).map_err(|e| RemoteError::file(dir, e))?;
                // The certificate goes last: a crash in between leaves half an
                // identity, which is refused above rather than silently replaced.
                write_private(&kp, &id.key_pkcs8).map_err(|e| RemoteError::file(&kp, e))?;
                ds_fsutil::write_atomic(&cp, &id.cert_der)
                    .map_err(|e| RemoteError::file(&cp, e))?;
                Ok(id)
            }
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub(crate) fn cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

/// Atomic owner-only (0600) write: temp → fsync → rename.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = ds_fsutil::temp_path(dir, "tmp");
    let res = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn identity_is_made_once_and_reloaded() {
        let t = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(t.path()).unwrap();
        let b = Identity::load_or_create(t.path()).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        let mode = fs::metadata(t.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the private key is owner-only");
        assert_ne!(
            Identity::generate().unwrap().fingerprint(),
            a.fingerprint(),
            "fresh keys differ"
        );
        assert!(format!("{a:?}").contains("Fingerprint("));
    }

    #[test]
    fn half_an_identity_is_refused_not_replaced() {
        let t = tempfile::tempdir().unwrap();
        Identity::load_or_create(t.path()).unwrap();
        fs::remove_file(t.path().join(CERT_FILE)).unwrap();
        let key_before = fs::read(t.path().join(KEY_FILE)).unwrap();
        let e = Identity::load_or_create(t.path()).unwrap_err();
        assert!(e.to_string().contains("half an identity"), "{e}");
        assert_eq!(fs::read(t.path().join(KEY_FILE)).unwrap(), key_before);
        // A directory where the key should be: an I/O error, not a new key.
        let u = tempfile::tempdir().unwrap();
        fs::create_dir(u.path().join(KEY_FILE)).unwrap();
        assert!(matches!(
            Identity::load_or_create(u.path()),
            Err(RemoteError::File { .. })
        ));
        // A certificate that is not Ed25519.
        fs::write(t.path().join(CERT_FILE), b"not a cert").unwrap();
        assert!(matches!(
            Identity::load_or_create(t.path()),
            Err(RemoteError::Identity(_))
        ));
    }

    #[test]
    fn fingerprints_parse_print_and_need_exactly_one_key() {
        let id = Identity::generate().unwrap();
        let fp = id.fingerprint();
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(fp.to_string(), fp.to_hex());
        assert_eq!(Fingerprint::from_hex("zz"), None);
        let short = fp.short();
        assert_eq!(short.len(), 19);
        assert!(fp.to_hex().starts_with(&short.replace(' ', "")));
        assert_eq!(Fingerprint::of_cert(b"not a certificate"), None);
        let cut = &id.cert_der[..id.cert_der.len() - 1];
        assert_eq!(Fingerprint::of_cert(cut), None, "truncated DER");
        // A certificate for another key type is refused.
        let p256 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let other = rcgen::CertificateParams::new(vec!["x".to_owned()])
            .unwrap()
            .self_signed(&p256)
            .unwrap();
        assert_eq!(Fingerprint::of_cert(other.der()), None);
    }
}
