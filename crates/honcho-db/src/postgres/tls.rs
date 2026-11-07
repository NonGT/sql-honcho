//! The rustls setup for each TLS mode.

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{DbError, Result, TlsConfig, TlsMode};

pub fn connector(tls: &TlsConfig) -> Result<(MakeRustlsConnect, SslMode)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = &tls.ca_file {
        let certs = certificates(ca)?;
        if certs.is_empty() {
            return Err(DbError::Tls(format!("no certificates in {}", ca.display())));
        }
        for cert in certs {
            roots.add(cert).map_err(error)?;
        }
    }
    let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| DbError::Tls(e.to_string()))?;
    let name = match &tls.server_name {
        Some(n) => Some(
            ServerName::try_from(n.clone())
                .map_err(|_| DbError::Tls(format!("{n} isn't a host name")))?,
        ),
        None => None,
    };
    let verifier = Verifier {
        webpki,
        provider: provider.clone(),
        mode: tls.mode,
        name,
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(error)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));
    let config = match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => {
            let key = PrivateKeyDer::from_pem_file(key)
                .map_err(|e| DbError::Tls(format!("{}: {e}", key.display())))?;
            builder
                .with_client_auth_cert(certificates(cert)?, key)
                .map_err(error)?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(DbError::Tls(
                "a client certificate needs both the certificate and its key".into(),
            ))
        }
    };
    let mode = match tls.mode {
        TlsMode::Disable => SslMode::Disable,
        TlsMode::Prefer => SslMode::Prefer,
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
    };
    Ok((MakeRustlsConnect::new(config), mode))
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect())
        .map_err(|e| DbError::Tls(format!("{}: {e}", path.display())))
}

fn error(e: rustls::Error) -> DbError {
    DbError::Tls(e.to_string())
}

/// Checks as much as the mode asks: nothing below verify-ca, the chain for
/// verify-ca, and the chain and name for verify-full. The name is the real
/// host's when going through a tunnel.
#[derive(Debug)]
struct Verifier {
    webpki: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
    mode: TlsMode,
    name: Option<ServerName<'static>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if !matches!(self.mode, TlsMode::VerifyCa | TlsMode::VerifyFull) {
            return Ok(ServerCertVerified::assertion());
        }
        let name = self.name.as_ref().unwrap_or(server_name);
        match self
            .webpki
            .verify_server_cert(end_entity, intermediates, name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) if self.mode == TlsMode::VerifyCa => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
