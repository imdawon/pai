//! Certificate pinning for the model API. The relay holds the IP address that `api.<domain>`
//! points to, so it could get a valid certificate for that name and read requests. Checking the
//! server's public key against pins that ship with `pai` stops that, whatever certificate is shown.

use std::sync::Arc;

use base64::Engine;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
#[cfg(feature = "test-mode")]
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Shown when the server's key isn't pinned; `proxy` looks for it in the error chain.
pub const PIN_MISMATCH: &str = "the model API's key doesn't match pai's pins";

/// A pin: base64 of the SHA-256 of the certificate's SubjectPublicKeyInfo (as in HPKP's
/// `pin-sha256`). Make one with:
/// `openssl x509 -pubkey -noout -in cert.pem | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64`
pub fn spki_pin(cert: &CertificateDer<'_>) -> Option<String> {
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    let spki = ee.subject_public_key_info();
    Some(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(spki.as_ref())))
}

#[derive(Debug)]
struct PinnedVerifier {
    inner: Arc<WebPkiServerVerifier>,
    pins: Vec<String>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        match spki_pin(end_entity) {
            Some(pin) if self.pins.contains(&pin) => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::General(PIN_MISMATCH.into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn roots(test_ca: Option<&std::path::Path>) -> Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    match test_ca {
        #[cfg(not(feature = "test-mode"))]
        Some(_) => return Err(Error::Setup("this pai is a release build, which has no test mode".into())),
        #[cfg(feature = "test-mode")]
        Some(path) => {
            let bad = |what: String| Error::Setup(format!("the test CA {} {what}", path.display()));
            for cert in CertificateDer::pem_file_iter(path).map_err(|e| bad(format!("couldn't be read: {e}")))? {
                let cert = cert.map_err(|e| bad(format!("isn't valid PEM: {e}")))?;
                roots.add(cert).map_err(|e| bad(format!("has a certificate rustls can't use: {e}")))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(roots)
}

/// TLS settings for the relay: the public certificate authorities, or only the test CA in test
/// mode. The relay isn't pinned: it only ever sees blinded requests and encrypted chats.
pub fn relay_config(test_ca: Option<&std::path::Path>) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Setup(format!("couldn't set up TLS: {e}")))?
        .with_root_certificates(roots(test_ca)?)
        .with_no_client_auth())
}

/// TLS settings for the model API: the usual certificate checks, plus the pins. In test mode
/// `test_ca` is trusted instead of the public certificate authorities.
pub fn client_config(pins: &[String], test_ca: Option<&std::path::Path>) -> Result<rustls::ClientConfig> {
    if pins.is_empty() {
        return Err(Error::Setup(
            "pai has no pins for the model API, so it can't check it's talking to the real one. \
             This build of pai is incomplete; install a release"
                .into(),
        ));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots(test_ca)?), provider.clone())
        .build()
        .map_err(|e| Error::Setup(format!("couldn't set up certificate checks: {e}")))?;
    let verifier = Arc::new(PinnedVerifier { inner, pins: pins.to_vec() });
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Setup(format!("couldn't set up TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(cfg)
}
