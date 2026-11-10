//! TLS for device links, with KDE Connect's trust model.
//!
//! Every device presents a long-lived self-signed certificate in both TLS
//! roles. There are no certificate authorities and no host names: before
//! pairing, any certificate is accepted; after pairing, the peer must present
//! exactly the certificate pinned at pairing. Either way the peer must prove
//! it holds the certificate's private key, which rustls checks through the
//! handshake signature.
//!
//! Which side is the TLS client is decided by the link layer, so both roles
//! are built here.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::NoServerSessionStorage;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, CommonState, DigitallySignedStruct, DistinguishedName,
    ServerConfig, SignatureScheme, SupportedProtocolVersion,
};
use tracing::warn;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::store::LocalIdentity;

// The Android app only speaks TLS 1.2; desktop peers may prefer 1.3.
static PROTOCOL_VERSIONS: &[&SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("could not build the TLS configuration: {0}")]
    Config(#[from] rustls::Error),
}

/// What the peer's certificate must be for a handshake to succeed.
#[derive(Clone, PartialEq, Eq)]
pub enum PeerTrust {
    /// Any certificate, for a device that is not paired yet. The peer still
    /// has to hold the private key of whatever certificate it presents.
    Unverified,
    /// Exactly this DER certificate, pinned when the device was paired.
    Pinned(Vec<u8>),
}

impl fmt::Debug for PeerTrust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unverified => f.write_str("Unverified"),
            Self::Pinned(der) => write!(f, "Pinned({} bytes)", der.len()),
        }
    }
}

/// Configuration for the TLS server role. A client certificate is mandatory.
pub fn server_config(
    local: &LocalIdentity,
    trust: &PeerTrust,
) -> Result<Arc<ServerConfig>, TlsError> {
    server_config_with_versions(local, trust, PROTOCOL_VERSIONS)
}

fn server_config_with_versions(
    local: &LocalIdentity,
    trust: &PeerTrust,
    versions: &[&'static SupportedProtocolVersion],
) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = provider();
    let verifier = Arc::new(PeerVerifier::new(trust, &provider));
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificate_chain(local), private_key(local))?;
    disable_server_resumption(&mut config);
    Ok(Arc::new(config))
}

/// Configuration for the TLS client role. Our certificate is always presented.
/// Connect with [`server_name`].
pub fn client_config(
    local: &LocalIdentity,
    trust: &PeerTrust,
) -> Result<Arc<ClientConfig>, TlsError> {
    client_config_with_versions(local, trust, PROTOCOL_VERSIONS)
}

fn client_config_with_versions(
    local: &LocalIdentity,
    trust: &PeerTrust,
    versions: &[&'static SupportedProtocolVersion],
) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = provider();
    let verifier = Arc::new(PeerVerifier::new(trust, &provider));
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certificate_chain(local), private_key(local))?;
    // Peers are identified by certificate, never by name.
    config.enable_sni = false;
    // A resumed session skips certificate verification, and the trust that
    // applied when it was established may since have changed (pairing,
    // unpairing). Links are long-lived, so full handshakes cost nothing.
    config.resumption = Resumption::disabled();
    Ok(Arc::new(config))
}

fn disable_server_resumption(config: &mut ServerConfig) {
    // See the client side: resumption would carry stale trust decisions.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
}

/// The server name to pass when connecting as TLS client. Peers have no names,
/// so this is a fixed placeholder that the verifier ignores. It is an IP
/// address so that rustls never sends it as SNI.
pub fn server_name() -> ServerName<'static> {
    ServerName::IpAddress(IpAddr::V4(Ipv4Addr::UNSPECIFIED).into())
}

/// The peer's end-entity certificate, DER encoded, once the handshake is done.
pub fn peer_certificate(state: &CommonState) -> Option<Vec<u8>> {
    state.peer_certificates()?.first().map(|der| der.to_vec())
}

/// [`peer_certificate`] for the stream types of `tokio_rustls`.
pub trait PeerCertificate {
    fn peer_certificate(&self) -> Option<Vec<u8>>;
}

impl<IO> PeerCertificate for tokio_rustls::client::TlsStream<IO> {
    fn peer_certificate(&self) -> Option<Vec<u8>> {
        peer_certificate(self.get_ref().1)
    }
}

impl<IO> PeerCertificate for tokio_rustls::server::TlsStream<IO> {
    fn peer_certificate(&self) -> Option<Vec<u8>> {
        peer_certificate(self.get_ref().1)
    }
}

impl<IO> PeerCertificate for tokio_rustls::TlsStream<IO> {
    fn peer_certificate(&self) -> Option<Vec<u8>> {
        peer_certificate(self.get_ref().1)
    }
}

// Built explicitly rather than taken from a process-wide default, so that a
// host process that installs another provider (or none) cannot change ours.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certificate_chain(local: &LocalIdentity) -> Vec<CertificateDer<'static>> {
    vec![local.certificate_der().clone()]
}

fn private_key(local: &LocalIdentity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(local.private_key_der().clone_key())
}

/// Checks the peer's certificate in either TLS role.
///
/// Name, validity dates, extensions, and the certificate's own signature are
/// deliberately not checked: KDE Connect certificates are self-signed, carry
/// no names, and trust comes from pairing, not from the certificate.
struct PeerVerifier {
    pinned: Option<Vec<u8>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PeerVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerVerifier")
            .field("pinned", &self.pinned.is_some())
            .finish_non_exhaustive()
    }
}

impl PeerVerifier {
    fn new(trust: &PeerTrust, provider: &CryptoProvider) -> Self {
        Self {
            pinned: match trust {
                PeerTrust::Unverified => None,
                PeerTrust::Pinned(der) => Some(der.clone()),
            },
            // The provider's full set covers ECDSA (P-256, P-384), Ed25519, and
            // RSA PKCS#1 and PSS, so both EC and older RSA peers can sign.
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn verify(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match &self.pinned {
            Some(pinned) if pinned.as_slice() != end_entity.as_ref() => {
                warn!("peer presented a certificate other than the one pinned at pairing");
                Err(CertificateError::ApplicationVerificationFailure.into())
            }
            // The pinned certificate parsed when it was stored; parsing an
            // unknown one up front gives a clear error instead of a confusing
            // failure later in signature verification.
            Some(_) => Ok(()),
            None => match X509Certificate::from_der(end_entity) {
                Ok(([], _)) => Ok(()),
                _ => Err(CertificateError::BadEncoding.into()),
            },
        }
    }

    fn verify_tls12(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.verify(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    // No hints: peers have no CA to name, and an empty list tells the client
    // to send whatever certificate it has.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.verify(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::ProtocolVersion;
    use rustls::client::ResolvesClientCert;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use std::io;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector, client, server};

    type Handshake = (
        io::Result<server::TlsStream<DuplexStream>>,
        io::Result<client::TlsStream<DuplexStream>>,
    );

    async fn handshake(server: Arc<ServerConfig>, client: Arc<ClientConfig>) -> Handshake {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let accept = TlsAcceptor::from(server).accept(server_io);
        let connect = TlsConnector::from(client).connect(server_name(), client_io);
        tokio::join!(accept, connect)
    }

    fn identities() -> (LocalIdentity, LocalIdentity) {
        (
            LocalIdentity::generate().unwrap(),
            LocalIdentity::generate().unwrap(),
        )
    }

    fn pinned(identity: &LocalIdentity) -> PeerTrust {
        PeerTrust::Pinned(identity.certificate_der().to_vec())
    }

    /// The rustls error behind an I/O error from a failed handshake.
    fn rustls_error(error: &io::Error) -> Option<&rustls::Error> {
        error.get_ref()?.downcast_ref()
    }

    async fn assert_data_flows(
        mut server: server::TlsStream<DuplexStream>,
        mut client: client::TlsStream<DuplexStream>,
    ) {
        client.write_all(b"ping\n").await.unwrap();
        client.flush().await.unwrap();
        let mut buffer = [0; 5];
        server.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"ping\n");
        server.write_all(b"pong\n").await.unwrap();
        server.flush().await.unwrap();
        client.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"pong\n");
    }

    #[tokio::test]
    async fn unverified_both_ways_connects_and_exposes_certificates() {
        let (a, b) = identities();
        let (server, client) = handshake(
            server_config(&a, &PeerTrust::Unverified).unwrap(),
            client_config(&b, &PeerTrust::Unverified).unwrap(),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());
        assert_eq!(
            server.peer_certificate().as_deref(),
            Some(b.certificate_der().as_ref())
        );
        assert_eq!(
            client.peer_certificate().as_deref(),
            Some(a.certificate_der().as_ref())
        );
        // Both sides allow 1.3, so that is what they agree on.
        assert_eq!(
            client.get_ref().1.protocol_version(),
            Some(ProtocolVersion::TLSv1_3)
        );
        // The enum stream type works too.
        let server = tokio_rustls::TlsStream::from(server);
        assert_eq!(
            server.peer_certificate().as_deref(),
            Some(b.certificate_der().as_ref())
        );
    }

    #[tokio::test]
    async fn data_flows_after_the_handshake() {
        let (a, b) = identities();
        let (server, client) = handshake(
            server_config(&a, &pinned(&b)).unwrap(),
            client_config(&b, &pinned(&a)).unwrap(),
        )
        .await;
        assert_data_flows(server.unwrap(), client.unwrap()).await;
    }

    #[tokio::test]
    async fn pinned_to_the_right_certificate_connects() {
        let (a, b) = identities();
        for versions in [PROTOCOL_VERSIONS, &[&rustls::version::TLS12]] {
            let (server, client) = handshake(
                server_config_with_versions(&a, &pinned(&b), versions).unwrap(),
                client_config(&b, &pinned(&a)).unwrap(),
            )
            .await;
            server.unwrap();
            client.unwrap();
        }
    }

    #[tokio::test]
    async fn server_pinned_to_another_certificate_rejects_the_client() {
        let (a, b) = identities();
        let stranger = LocalIdentity::generate().unwrap();
        for versions in [PROTOCOL_VERSIONS, &[&rustls::version::TLS12]] {
            let (server, _client) = handshake(
                server_config_with_versions(&a, &pinned(&stranger), versions).unwrap(),
                client_config(&b, &PeerTrust::Unverified).unwrap(),
            )
            .await;
            let error = server.unwrap_err();
            assert_eq!(
                rustls_error(&error),
                Some(&rustls::Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure
                )),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn client_pinned_to_another_certificate_rejects_the_server() {
        let (a, b) = identities();
        let stranger = LocalIdentity::generate().unwrap();
        for versions in [PROTOCOL_VERSIONS, &[&rustls::version::TLS12]] {
            let (server, client) = handshake(
                server_config(&a, &PeerTrust::Unverified).unwrap(),
                client_config_with_versions(&b, &pinned(&stranger), versions).unwrap(),
            )
            .await;
            let error = client.unwrap_err();
            assert_eq!(
                rustls_error(&error),
                Some(&rustls::Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure
                )),
                "{error}"
            );
            assert!(server.is_err());
        }
    }

    #[tokio::test]
    async fn a_peer_limited_to_tls_1_2_still_connects() {
        let (a, b) = identities();
        let tls12: &[&'static SupportedProtocolVersion] = &[&rustls::version::TLS12];

        // As the TLS client, which is the Android app's usual role.
        let (server, client) = handshake(
            server_config(&a, &PeerTrust::Unverified).unwrap(),
            client_config_with_versions(&b, &PeerTrust::Unverified, tls12).unwrap(),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());
        assert_eq!(
            server.get_ref().1.protocol_version(),
            Some(ProtocolVersion::TLSv1_2)
        );
        assert_eq!(
            server.peer_certificate().as_deref(),
            Some(b.certificate_der().as_ref())
        );
        assert_data_flows(server, client).await;

        // And as the TLS server.
        let (server, client) = handshake(
            server_config_with_versions(&a, &pinned(&b), tls12).unwrap(),
            client_config(&b, &pinned(&a)).unwrap(),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());
        assert_eq!(
            client.get_ref().1.protocol_version(),
            Some(ProtocolVersion::TLSv1_2)
        );
        assert_data_flows(server, client).await;
    }

    #[tokio::test]
    async fn a_client_without_a_certificate_is_rejected() {
        let a = LocalIdentity::generate().unwrap();
        let anonymous = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(PROTOCOL_VERSIONS)
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier(&PeerTrust::Unverified))
            .with_no_client_auth();
        let (server, _client) = handshake(
            server_config(&a, &PeerTrust::Unverified).unwrap(),
            Arc::new(anonymous),
        )
        .await;
        let error = server.unwrap_err();
        assert_eq!(
            rustls_error(&error),
            Some(&rustls::Error::NoCertificatesPresented),
            "{error}"
        );
    }

    /// Presents one identity's certificate but signs with another's key: a
    /// peer that copied a paired device's certificate (TLS 1.2 sends it in the
    /// clear) without having its private key.
    #[derive(Debug)]
    struct Impostor(Arc<CertifiedKey>);

    impl Impostor {
        fn new(certificate: &LocalIdentity, key: &LocalIdentity) -> Arc<Self> {
            let key = provider()
                .key_provider
                .load_private_key(private_key(key))
                .unwrap();
            let certified = CertifiedKey::new(certificate_chain(certificate), key);
            Arc::new(Self(Arc::new(certified)))
        }
    }

    impl ResolvesClientCert for Impostor {
        fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }

        fn has_certs(&self) -> bool {
            true
        }
    }

    impl ResolvesServerCert for Impostor {
        fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }
    }

    fn verifier(trust: &PeerTrust) -> Arc<PeerVerifier> {
        Arc::new(PeerVerifier::new(trust, &provider()))
    }

    #[tokio::test]
    async fn a_client_with_a_copied_certificate_is_rejected() {
        let (a, b) = identities();
        let thief = LocalIdentity::generate().unwrap();
        for versions in [PROTOCOL_VERSIONS, &[&rustls::version::TLS12]] {
            let impostor = ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(versions)
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(verifier(&PeerTrust::Unverified))
                .with_client_cert_resolver(Impostor::new(&b, &thief));
            let (server, _client) =
                handshake(server_config(&a, &pinned(&b)).unwrap(), Arc::new(impostor)).await;
            let error = server.unwrap_err();
            assert_eq!(
                rustls_error(&error),
                Some(&rustls::Error::InvalidCertificate(
                    CertificateError::BadSignature
                )),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_server_with_a_copied_certificate_is_rejected() {
        let (a, b) = identities();
        let thief = LocalIdentity::generate().unwrap();
        for versions in [PROTOCOL_VERSIONS, &[&rustls::version::TLS12]] {
            let impostor = ServerConfig::builder_with_provider(provider())
                .with_protocol_versions(versions)
                .unwrap()
                .with_client_cert_verifier(verifier(&PeerTrust::Unverified))
                .with_cert_resolver(Impostor::new(&a, &thief));
            let (_server, client) =
                handshake(Arc::new(impostor), client_config(&b, &pinned(&a)).unwrap()).await;
            let error = client.unwrap_err();
            assert_eq!(
                rustls_error(&error),
                Some(&rustls::Error::InvalidCertificate(
                    CertificateError::BadSignature
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn unparseable_certificates_are_rejected_before_pairing() {
        let verifier = PeerVerifier::new(&PeerTrust::Unverified, &provider());
        let garbage = CertificateDer::from(&b"not a certificate"[..]);
        assert_eq!(
            verifier.verify(&garbage),
            Err(CertificateError::BadEncoding.into())
        );
    }
}
