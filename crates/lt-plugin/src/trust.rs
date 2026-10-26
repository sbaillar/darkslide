//! Content hashes and signatures.
//!
//! A plug-in's identity on disk is the BLAKE3 of every file in its package (relative
//! path → hash, see `Package::hashes`). `SIGNATURE` holds an Ed25519 signature over that
//! list, made with `lt-plugin sign`. The host pins the list at install, and pins the
//! signing key per plug-in id (a package signed by another key is another plug-in).
//!
//! Private keys never enter the repository: authors keep theirs, and Luxtable's own
//! bundled plug-ins need none (their hashes are compiled into the app, see `build.rs`).

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::PluginError;
use crate::package::Package;

/// The signature file's name in a plug-in (not itself hashed).
pub const SIGNATURE_FILE: &str = "SIGNATURE";
/// Domain prefix of the signed message.
const DOMAIN: &[u8] = b"luxtable-plugin-sig-v1\0";
/// Migration source: the domain of signatures made when the app was called Darkslide.
/// Plug-ins signed then still verify (new signatures always use [`DOMAIN`]).
const LEGACY_DOMAIN: &[u8] = b"darkslide-plugin-sig-v1\0";

/// Relative path → BLAKE3 hex (`+x` for executables), byte-sorted.
pub type FileHashes = BTreeMap<String, String>;

/// The `SIGNATURE` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureFile {
    pub format: u32,
    /// The name the signer gives itself. Shown only as "claims to be"; the key's
    /// fingerprint is the identity.
    pub signer: String,
    /// Ed25519 public key, hex.
    pub key: String,
    /// Ed25519 signature over the file list, hex.
    pub signature: String,
}

/// A signing key file written by `lt-plugin keygen` (keep it private; never commit it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyFile {
    pub format: u32,
    pub signer: String,
    pub key: String,
    pub secret: String,
}

/// A verified signer: the name it claims and its public key (hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    pub name: String,
    pub key: String,
}

impl Signer {
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.key)
    }
}

/// A key's fingerprint for people to compare: 16 hex digits of its BLAKE3 in groups
/// (`3F2A-91C0-7D44-E815`).
#[must_use]
pub fn fingerprint(key_hex: &str) -> String {
    let h = blake3::hash(key_hex.as_bytes())
        .to_hex()
        .to_ascii_uppercase();
    [&h[0..4], &h[4..8], &h[8..12], &h[12..16]].join("-")
}

/// What a signature covers: the domain, the signer's claimed name and key (so a
/// relabelled signature fails), then every file.
fn message(domain: &[u8], files: &FileHashes, signer: &str, key: &str) -> Vec<u8> {
    let mut m = domain.to_vec();
    for part in [signer, key] {
        m.extend_from_slice(part.as_bytes());
        m.push(0);
    }
    for (path, hash) in files {
        m.extend_from_slice(path.as_bytes());
        m.push(0);
        m.extend_from_slice(hash.as_bytes());
        m.push(b'\n');
    }
    m
}

/// A new key pair for `signer`.
pub fn keygen(signer: &str) -> Result<KeyFile, PluginError> {
    if signer.trim().is_empty() || crate::text::reserved_name(signer) {
        return Err(PluginError::Invalid(format!(
            "`{signer}` can't be a signer name"
        )));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| PluginError::Invalid(format!("no random source: {e}")))?;
    let sk = SigningKey::from_bytes(&seed);
    Ok(KeyFile {
        format: 1,
        signer: signer.trim().to_owned(),
        key: hex(sk.verifying_key().as_bytes()),
        secret: hex(&seed),
    })
}

/// Sign `files` with `key`.
pub fn sign(files: &FileHashes, key: &KeyFile) -> Result<SignatureFile, PluginError> {
    let seed: [u8; 32] = unhex(&key.secret)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            PluginError::Invalid("the key file's secret is not 32 bytes of hex".into())
        })?;
    let sk = SigningKey::from_bytes(&seed);
    if hex(sk.verifying_key().as_bytes()) != key.key {
        return Err(PluginError::Invalid(
            "the key file's public key doesn't match its secret".into(),
        ));
    }
    Ok(SignatureFile {
        format: 1,
        signer: key.signer.clone(),
        key: key.key.clone(),
        signature: hex(&sk
            .sign(&message(DOMAIN, files, &key.signer, &key.key))
            .to_bytes()),
    })
}

/// Check `sig` over `files`: the signer when it verifies.
pub fn verify(files: &FileHashes, sig: &SignatureFile) -> Result<Signer, PluginError> {
    let bad = |m: &str| PluginError::Signature(m.to_owned());
    if sig.format != 1 {
        return Err(bad("unknown signature format"));
    }
    // Checked on the name as it will be shown (zero-width and control characters gone).
    let shown = crate::text::clean(&sig.signer, 60);
    if crate::text::reserved_name(&shown) || shown.is_empty() || sig.signer.chars().count() > 60 {
        return Err(bad("the signer's name is reserved or not allowed"));
    }
    let key: [u8; 32] = unhex(&sig.key)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| bad("bad public key"))?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| bad("bad public key"))?;
    let s: [u8; 64] = unhex(&sig.signature)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| bad("bad signature"))?;
    let signature = Signature::from_bytes(&s);
    [DOMAIN, LEGACY_DOMAIN]
        .iter()
        .find(|d| {
            key.verify(&message(d, files, &sig.signer, &sig.key), &signature)
                .is_ok()
        })
        .ok_or_else(|| bad("the signature doesn't match the files (changed after signing?)"))?;
    Ok(Signer {
        name: crate::text::clean(&sig.signer, 60),
        key: sig.key.clone(),
    })
}

/// Verify the package's `SIGNATURE` against its files: `None` when there is none.
pub fn read_signature(pkg: &Package) -> Result<Option<Signer>, PluginError> {
    let Some(bytes) = pkg.get(SIGNATURE_FILE) else {
        return Ok(None);
    };
    let sig: SignatureFile = serde_json::from_slice(bytes)
        .map_err(|e| PluginError::Signature(format!("SIGNATURE: {e}")))?;
    verify(&pkg.hashes(), &sig).map(Some)
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[must_use]
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// A signature as the app made them before the rename (tests of the old domain).
#[cfg(test)]
pub(crate) fn sign_legacy_for_tests(files: &FileHashes, key: &KeyFile) -> SignatureFile {
    let seed: [u8; 32] = unhex(&key.secret).unwrap().try_into().unwrap();
    let sk = SigningKey::from_bytes(&seed);
    SignatureFile {
        format: 1,
        signer: key.signer.clone(),
        key: key.key.clone(),
        signature: hex(&sk
            .sign(&message(LEGACY_DOMAIN, files, &key.signer, &key.key))
            .to_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn folder() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("plugin.toml"), "x").unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/a.json"), "[]").unwrap();
        d
    }

    fn sign_into(dir: &std::path::Path, key: &KeyFile) -> SignatureFile {
        let sig = sign(&Package::read_dir(dir).unwrap().hashes(), key).unwrap();
        fs::write(
            dir.join(SIGNATURE_FILE),
            serde_json::to_string(&sig).unwrap(),
        )
        .unwrap();
        sig
    }

    #[test]
    fn sign_and_verify_round_trip_and_detect_changes() {
        let d = folder();
        let key = keygen("Tester").unwrap();
        let sig = sign_into(d.path(), &key);
        let signer = read_signature(&Package::read_dir(d.path()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            signer,
            Signer {
                name: "Tester".into(),
                key: key.key.clone()
            }
        );
        assert_eq!(signer.fingerprint().len(), 19);

        fs::write(d.path().join("sub/a.json"), "[1]").unwrap();
        let e = read_signature(&Package::read_dir(d.path()).unwrap()).unwrap_err();
        assert!(e.to_string().contains("doesn't match"), "{e}");
        let files = Package::read_dir(d.path()).unwrap().hashes();
        let other = keygen("Mallory").unwrap();
        assert!(
            verify(
                &files,
                &SignatureFile {
                    key: other.key,
                    ..sig.clone()
                }
            )
            .is_err()
        );
        assert!(
            verify(
                &files,
                &SignatureFile {
                    format: 2,
                    ..sig.clone()
                }
            )
            .is_err()
        );
        assert!(
            verify(
                &files,
                &SignatureFile {
                    key: "zz".into(),
                    ..sig.clone()
                }
            )
            .is_err()
        );
        assert!(
            verify(
                &files,
                &SignatureFile {
                    signature: "00".into(),
                    ..sig.clone()
                }
            )
            .is_err()
        );
        // The claimed name is signed too: relabelling a signature breaks it.
        let relabelled = SignatureFile {
            signer: "Victim Corp".into(),
            ..sig.clone()
        };
        assert!(
            verify(&files, &relabelled)
                .unwrap_err()
                .to_string()
                .contains("doesn't match")
        );
        // A zero-width space doesn't get a reserved name past the check.
        let hidden = SignatureFile {
            signer: "Dark\u{200B}slide".into(),
            ..sig.clone()
        };
        assert!(
            verify(&files, &hidden)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        let luxtable = SignatureFile {
            signer: "Luxtable Team".into(),
            ..sig
        };
        assert!(
            verify(&files, &luxtable)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }

    #[test]
    fn the_exec_bit_is_signed() {
        use std::os::unix::fs::PermissionsExt;
        let d = folder();
        let key = keygen("Tester").unwrap();
        sign_into(d.path(), &key);
        fs::set_permissions(
            d.path().join("sub/a.json"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(read_signature(&Package::read_dir(d.path()).unwrap()).is_err());
    }

    #[test]
    fn missing_or_malformed_signature() {
        let d = folder();
        assert_eq!(
            read_signature(&Package::read_dir(d.path()).unwrap()).unwrap(),
            None
        );
        fs::write(d.path().join(SIGNATURE_FILE), "not json").unwrap();
        assert!(read_signature(&Package::read_dir(d.path()).unwrap()).is_err());
    }

    #[test]
    fn key_files_and_names_are_checked() {
        let files = FileHashes::new();
        let mut key = keygen("A").unwrap();
        key.key = keygen("B").unwrap().key;
        assert!(
            sign(&files, &key)
                .unwrap_err()
                .to_string()
                .contains("doesn't match")
        );
        key.secret = "abc".into();
        assert!(sign(&files, &key).is_err());
        assert!(keygen("Luxtable").is_err());
        assert!(keygen(" ").is_err());
        assert_eq!(fingerprint("ab"), fingerprint("ab"));
        assert_ne!(fingerprint("ab"), fingerprint("ac"));
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("+f"), None, "from_str_radix would take a sign");
    }
}
