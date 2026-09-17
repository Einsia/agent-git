//! Approved certificates bind endpoints independently of the relay connection.

use anyhow::{Context, ensure};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::WebPkiClientVerifier,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const SERVER_NAME: &str = "agit-peer";
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct PeerCertificate(Vec<u8>);

impl std::fmt::Debug for PeerCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PeerCertificate")
            .field(&self.fingerprint())
            .finish()
    }
}

impl PeerCertificate {
    pub fn from_der(der: Vec<u8>) -> anyhow::Result<Self> {
        let certificate = Self(der);
        certificate.roots()?;
        Ok(certificate)
    }

    pub fn as_der(&self) -> &[u8] {
        &self.0
    }

    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(&self.0))
    }

    fn roots(&self) -> anyhow::Result<RootCertStore> {
        ensure!(
            !self.0.is_empty() && self.0.len() <= MAX_CERTIFICATE_BYTES,
            "invalid peer certificate length"
        );
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.0.clone()))
            .context("invalid peer certificate")?;
        Ok(roots)
    }
}

pub struct Identity {
    certificate: PeerCertificate,
    key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn generate() -> anyhow::Result<Self> {
        let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
        Self::from_der(
            generated.cert.der().to_vec(),
            generated.key_pair.serialize_der(),
        )
    }

    pub fn from_der(certificate: Vec<u8>, private_key: Vec<u8>) -> anyhow::Result<Self> {
        let identity = Self {
            certificate: PeerCertificate::from_der(certificate)?,
            key: PrivatePkcs8KeyDer::from(private_key).into(),
        };
        identity.server_config(&identity.certificate)?;
        Ok(identity)
    }

    pub fn certificate(&self) -> &PeerCertificate {
        &self.certificate
    }

    pub fn private_key_der(&self) -> &[u8] {
        self.key.secret_der()
    }

    fn client_config(&self, peer: &PeerCertificate) -> anyhow::Result<ClientConfig> {
        Ok(
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_root_certificates(peer.roots()?)
                .with_client_auth_cert(
                    vec![CertificateDer::from(self.certificate.0.clone())],
                    self.key.clone_key(),
                )?,
        )
    }

    fn server_config(&self, peer: &PeerCertificate) -> anyhow::Result<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(peer.roots()?), provider.clone())
                .build()?;
        Ok(ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(self.certificate.0.clone())],
                self.key.clone_key(),
            )?)
    }

    pub async fn connect<S>(
        &self,
        peer: &PeerCertificate,
        stream: S,
    ) -> anyhow::Result<tokio_rustls::client::TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        tokio::time::timeout(HANDSHAKE_TIMEOUT, self.connecting(peer, stream)?)
            .await
            .context("peer authentication timed out")?
            .context("peer authentication failed")
    }

    // Callers bound relay readiness separately and start the TLS deadline after readiness.
    pub(crate) fn connecting<S>(
        &self,
        peer: &PeerCertificate,
        stream: S,
    ) -> anyhow::Result<tokio_rustls::Connect<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let connector = TlsConnector::from(Arc::new(self.client_config(peer)?));
        Ok(connector.connect(ServerName::try_from(SERVER_NAME)?, stream))
    }

    pub async fn accept<S>(
        &self,
        peer: &PeerCertificate,
        stream: S,
    ) -> anyhow::Result<tokio_rustls::server::TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let acceptor = TlsAcceptor::from(Arc::new(self.server_config(peer)?));
        tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
            .await
            .context("peer authentication timed out")?
            .context("peer authentication failed")
    }
}
