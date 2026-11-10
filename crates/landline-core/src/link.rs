//! Turning a TCP connection into an authenticated link with one device.
//!
//! KDE Connect inverts the TLS roles: the device that *receives* a UDP
//! identity opens the TCP connection and then acts as the TLS **server**; the
//! device that accepted the connection acts as the TLS **client**. Under
//! protocol 8 both sides repeat their identity inside the TLS session, and
//! only that copy is trusted.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::identity::{self, Identity};
use crate::packet::Packet;
use crate::store::{self, LocalIdentity};
use crate::tls::{self, PeerCertificate, PeerTrust};

/// Every step of a handshake must finish within this, as on the peers.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest identity packet a peer may send. The KDE Connect apps use the
/// same limit.
const MAX_IDENTITY_SIZE: usize = 512 * 1024;

/// Largest packet accepted on an established link. Payloads (files, icons)
/// travel on their own connections, so packets stay small; the KDE Connect
/// apps allow 32 MiB, far more than any packet body they send.
pub(crate) const MAX_PACKET_SIZE: usize = 8 * 1024 * 1024;

pub(crate) type LinkReader = BufReader<ReadHalf<TlsStream<TcpStream>>>;
pub(crate) type LinkWriter = WriteHalf<TlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("timed out")]
    Timeout,
    #[error("the peer's identity is malformed: {0}")]
    Identity(String),
    #[error("the connection was meant for another device")]
    WrongTarget,
    #[error("the peer changed its {0} during the handshake")]
    IdentityChanged(&'static str),
    #[error("the peer speaks protocol {offered}, older than the {known} it used when paired")]
    ProtocolDowngrade { offered: i64, known: i64 },
    #[error("the peer presented no certificate")]
    NoPeerCertificate,
    #[error("the peer's certificate was issued to another device")]
    CertificateMismatch,
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

/// What this side needs to know to authenticate a peer by device ID.
pub(crate) trait TrustLookup: Send + Sync {
    /// The pinned certificate and last known protocol version of a paired
    /// device, or `None` if the device is not paired.
    fn pinned(&self, device_id: &str) -> Option<(Vec<u8>, i64)>;
}

/// An authenticated connection to a device, ready for packets.
pub(crate) struct Link {
    pub identity: Identity,
    pub certificate: Vec<u8>,
    pub address: SocketAddr,
    pub reader: LinkReader,
    pub writer: LinkWriter,
}

/// We received `peer`'s UDP identity from `address`: connect back to its TCP
/// port, introduce ourselves in plaintext, then secure the connection as the
/// TLS server.
pub(crate) async fn connect(
    local: &LocalIdentity,
    our_identity: &Identity,
    trust: &dyn TrustLookup,
    peer: &Identity,
    address: SocketAddr,
) -> Result<Link, LinkError> {
    let trust = peer_trust(trust, peer)?;
    let mut stream = within(TcpStream::connect(address)).await??;
    configure(&stream);

    let mut hello = our_identity.clone();
    hello.tcp_port = None;
    hello.target_device_id = Some(peer.device_id.clone());
    hello.target_protocol_version = Some(peer.protocol_version);
    within(stream.write_all(hello.to_packet().to_line().as_bytes())).await??;

    let config = tls::server_config(local, &trust).map_err(|e| LinkError::Tls(e.to_string()))?;
    let stream = within(TlsAcceptor::from(config).accept(stream)).await??;
    secure(TlsStream::Server(stream), our_identity, peer, address).await
}

/// A device that received our UDP identity connected to our TCP listener:
/// read its plaintext identity, then secure the connection as the TLS client.
pub(crate) async fn accept(
    local: &LocalIdentity,
    our_identity: &Identity,
    trust: &dyn TrustLookup,
    mut stream: TcpStream,
    address: SocketAddr,
) -> Result<Link, LinkError> {
    configure(&stream);
    // Unbuffered on purpose: anything read past the newline would be lost to
    // the TLS handshake that follows on the same socket.
    let line = within(read_line_unbuffered(&mut stream, MAX_IDENTITY_SIZE)).await??;
    let peer = parse_identity(&line)?;

    if peer.device_id == our_identity.device_id {
        return Err(LinkError::WrongTarget);
    }
    if let Some(target) = &peer.target_device_id
        && target != &our_identity.device_id
    {
        return Err(LinkError::WrongTarget);
    }
    if let Some(version) = peer.target_protocol_version
        && version != identity::PROTOCOL_VERSION
    {
        return Err(LinkError::WrongTarget);
    }

    let trust = peer_trust(trust, &peer)?;
    let config = tls::client_config(local, &trust).map_err(|e| LinkError::Tls(e.to_string()))?;
    let connector = TlsConnector::from(config);
    let stream = within(connector.connect(tls::server_name(), stream)).await??;
    secure(TlsStream::Client(stream), our_identity, &peer, address).await
}

/// Finishes a handshake once TLS is up: exchange identities inside the
/// session and check the peer is still who it claimed to be in plaintext.
async fn secure(
    stream: TlsStream<TcpStream>,
    our_identity: &Identity,
    plaintext_peer: &Identity,
    address: SocketAddr,
) -> Result<Link, LinkError> {
    let certificate = stream
        .peer_certificate()
        .ok_or(LinkError::NoPeerCertificate)?;
    // A certificate's common name is its owner's device ID. Without this
    // check a device could claim another's ID with its own certificate and
    // be shown to the user under that ID.
    if store::common_name(&certificate).ok().as_deref() != Some(&plaintext_peer.device_id) {
        return Err(LinkError::CertificateMismatch);
    }

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Peers older than protocol 8 do not repeat their identity, so the
    // plaintext one is all there is.
    let identity = if plaintext_peer.protocol_version >= 8 {
        let mut hello = our_identity.clone();
        hello.tcp_port = None;
        within(writer.write_all(hello.to_packet().to_line().as_bytes())).await??;
        within(writer.flush()).await??;

        let line = within(read_line(&mut reader, MAX_IDENTITY_SIZE)).await??;
        let secure_peer = parse_identity(&line)?;
        if secure_peer.device_id != plaintext_peer.device_id {
            return Err(LinkError::IdentityChanged("device ID"));
        }
        if secure_peer.protocol_version != plaintext_peer.protocol_version {
            return Err(LinkError::IdentityChanged("protocol version"));
        }
        secure_peer
    } else {
        plaintext_peer.clone()
    };

    Ok(Link {
        identity,
        certificate,
        address,
        reader,
        writer,
    })
}

fn peer_trust(trust: &dyn TrustLookup, peer: &Identity) -> Result<PeerTrust, LinkError> {
    match trust.pinned(&peer.device_id) {
        Some((_, known)) if known > peer.protocol_version => Err(LinkError::ProtocolDowngrade {
            offered: peer.protocol_version,
            known,
        }),
        Some((certificate, _)) => Ok(PeerTrust::Pinned(certificate)),
        None => Ok(PeerTrust::Unverified),
    }
}

pub(crate) fn parse_identity(line: &str) -> Result<Identity, LinkError> {
    let packet = Packet::parse(line).map_err(|e| LinkError::Identity(e.to_string()))?;
    Identity::from_packet(&packet).map_err(|e| LinkError::Identity(e.to_string()))
}

/// There is no keepalive in the protocol, so a peer that vanishes (phone
/// asleep, Wi-Fi gone) is only noticed through TCP keepalive.
fn configure(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5));
    let socket = socket2::SockRef::from(stream);
    let _ = socket.set_tcp_keepalive(&keepalive);
    let _ = stream.set_nodelay(true);
}

async fn within<T>(future: impl Future<Output = T>) -> Result<T, LinkError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, future)
        .await
        .map_err(|_| LinkError::Timeout)
}

/// Reads one `\n`-terminated line of at most `max` bytes, without the
/// terminator.
pub(crate) async fn read_line<R>(reader: &mut R, max: usize) -> io::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len() + take > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            return into_string(line);
        }
    }
}

async fn read_line_unbuffered(stream: &mut TcpStream, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return into_string(line);
        }
        if line.len() == max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.push(byte);
    }
}

fn into_string(line: Vec<u8>) -> io::Result<String> {
    String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

/// Writes one packet to an established link.
pub(crate) async fn write_packet(writer: &mut LinkWriter, packet: &Packet) -> io::Result<()> {
    writer.write_all(packet.to_line().as_bytes()).await?;
    writer.flush().await
}

/// Shared by tests and the manager: a `TrustLookup` over anything callable.
impl<F> TrustLookup for F
where
    F: Fn(&str) -> Option<(Vec<u8>, i64)> + Send + Sync,
{
    fn pinned(&self, device_id: &str) -> Option<(Vec<u8>, i64)> {
        self(device_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_line_splits_on_newlines_across_reads() {
        let (mut tx, rx) = tokio::io::duplex(8);
        let mut reader = BufReader::with_capacity(4, rx);
        tokio::spawn(async move {
            tx.write_all(b"first line\nsecond\n").await.unwrap();
        });
        assert_eq!(read_line(&mut reader, 64).await.unwrap(), "first line");
        assert_eq!(read_line(&mut reader, 64).await.unwrap(), "second");
        let end = read_line(&mut reader, 64).await.unwrap_err();
        assert_eq!(end.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_line_rejects_lines_over_the_limit() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);
        tx.write_all(b"0123456789\n").await.unwrap();
        let error = read_line(&mut reader, 9).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_line_accepts_a_line_exactly_at_the_limit() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);
        tx.write_all(b"0123456789\n").await.unwrap();
        assert_eq!(read_line(&mut reader, 10).await.unwrap(), "0123456789");
    }

    #[test]
    fn a_paired_device_offering_an_older_protocol_is_refused() {
        let peer = Identity {
            device_id: "0123456789abcdef0123456789abcdef".into(),
            device_name: "Test Phone".into(),
            device_type: identity::DeviceType::Phone,
            protocol_version: 7,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            tcp_port: None,
            target_device_id: None,
            target_protocol_version: None,
        };
        let paired_at_8 = |_: &str| Some((vec![1, 2, 3], 8));
        assert!(matches!(
            peer_trust(&paired_at_8, &peer),
            Err(LinkError::ProtocolDowngrade {
                offered: 7,
                known: 8
            })
        ));
        let unpaired = |_: &str| None;
        assert!(matches!(
            peer_trust(&unpaired, &peer),
            Ok(PeerTrust::Unverified)
        ));
    }
}
