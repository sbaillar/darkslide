//! TLS 1.3 with mutual authentication and pinned Ed25519 keys.
//!
//! There is no CA: each side presents its self-signed device certificate,
//! proves it holds the key (the TLS 1.3 handshake signature, checked here
//! with the ring provider), and the other side accepts it only if the key's
//! [`Fingerprint`] is one it has pinned.
//!
//! - The server accepts a client key that is paired, or any key while a
//!   pairing window is open (that connection may then only pair).
//! - The client accepts the one server key it pinned, or, when pairing with
//!   a typed code and no QR, any key; pairing then binds that key into the
//!   SPAKE2 confirmation, so a substituted key fails the pairing.

use std::fmt;
use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};

use crate::Result;
use crate::identity::{Fingerprint, Identity};

/// The TLS server name clients send; certificates are pinned, not named.
pub const SERVER_NAME: &str = "luxtable";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn key_of(cert: &CertificateDer<'_>) -> Result<Fingerprint, Error> {
    Fingerprint::of_cert(cert).ok_or_else(|| Error::General("not an Ed25519 device key".into()))
}

fn check_signature(
    algs: &WebPkiSupportedAlgorithms,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    if dss.scheme != SignatureScheme::ED25519 {
        return Err(Error::General("device keys sign with Ed25519".into()));
    }
    verify_tls13_signature(message, cert, dss, algs)
}

/// Decides whether a client key may complete the handshake.
pub type ClientGate = Arc<dyn Fn(&Fingerprint) -> bool + Send + Sync>;

struct PinnedClients {
    gate: ClientGate,
    algs: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedClients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinnedClients")
    }
}

impl ClientCertVerifier for PinnedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        if (self.gate)(&key_of(end_entity)?) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::General("this device is not paired".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not offered".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        check_signature(&self.algs, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// What the client expects of the server's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    Key(Fingerprint),
    /// Pairing by typed code only: accept any key and remember it.
    AnyForPairing,
}

#[derive(Debug)]
pub(crate) struct PinnedServer {
    pin: Pin,
    pub(crate) seen: Mutex<Option<Fingerprint>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let fp = key_of(end_entity)?;
        if let Pin::Key(want) = self.pin
            && want != fp
        {
            return Err(Error::General(
                "the Mac's key does not match the paired key".into(),
            ));
        }
        *self.seen.lock().expect("pin lock") = Some(fp);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not offered".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        check_signature(&self.algs, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub(crate) fn server_config(id: &Identity, gate: ClientGate) -> Result<Arc<rustls::ServerConfig>> {
    let p = provider();
    let verifier = Arc::new(PinnedClients {
        gate,
        algs: p.signature_verification_algorithms,
    });
    let mut cfg = rustls::ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![id.cert()], id.key())?;
    // No resumption: a resumed session skips the client-key gate, which would
    // let a revoked key (or one let in while pairing was open) back in.
    cfg.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    cfg.send_tls13_tickets = 0;
    Ok(Arc::new(cfg))
}

pub(crate) fn client_config(
    id: &Identity,
    pin: Pin,
) -> Result<(Arc<rustls::ClientConfig>, Arc<PinnedServer>)> {
    let p = provider();
    let verifier = Arc::new(PinnedServer {
        pin,
        seen: Mutex::new(None),
        algs: p.signature_verification_algorithms,
    });
    let mut cfg = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_client_auth_cert(vec![id.cert()], id.key())?;
    cfg.resumption = rustls::client::Resumption::disabled();
    Ok((Arc::new(cfg), verifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dss() -> DigitallySignedStruct {
        // Only reachable through the handshake; a dummy ECDSA signature is enough
        // to check that anything but Ed25519 is refused before verification.
        // ECDSA P-256 / SHA-256, a one-byte signature.
        let bytes = [0x04, 0x03, 0x00, 0x01, 0x00];
        let mut r = rustls::internal::msgs::codec::Reader::init(&bytes);
        <DigitallySignedStruct as rustls::internal::msgs::codec::Codec>::read(&mut r).unwrap()
    }

    #[test]
    fn verifiers_refuse_tls12_other_schemes_and_foreign_keys() {
        let id = Identity::generate().unwrap();
        let algs = provider().signature_verification_algorithms;
        let clients = PinnedClients {
            gate: Arc::new(|_: &Fingerprint| false),
            algs,
        };
        let server = PinnedServer {
            pin: Pin::Key(id.fingerprint()),
            seen: Mutex::new(None),
            algs,
        };
        let cert = id.cert();
        let d = dss();
        assert!(clients.verify_tls12_signature(b"m", &cert, &d).is_err());
        assert!(server.verify_tls12_signature(b"m", &cert, &d).is_err());
        assert!(
            clients.verify_tls13_signature(b"m", &cert, &d).is_err(),
            "not Ed25519"
        );
        assert!(server.verify_tls13_signature(b"m", &cert, &d).is_err());
        assert_eq!(
            clients.supported_verify_schemes(),
            [SignatureScheme::ED25519]
        );
        assert_eq!(
            server.supported_verify_schemes(),
            [SignatureScheme::ED25519]
        );
        assert!(clients.root_hint_subjects().is_empty());
        assert_eq!(format!("{clients:?}"), "PinnedClients");
        let now = UnixTime::now();
        let e = clients.verify_client_cert(&cert, &[], now).unwrap_err();
        assert!(e.to_string().contains("not paired"), "{e}");
        let junk = CertificateDer::from(vec![1, 2, 3]);
        assert!(clients.verify_client_cert(&junk, &[], now).is_err());
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        assert!(
            server
                .verify_server_cert(&cert, &[], &name, &[], now)
                .is_ok()
        );
        let other = Identity::generate().unwrap().cert();
        let e = server
            .verify_server_cert(&other, &[], &name, &[], now)
            .unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
    }
}
