//! Payloads: bytes too large for a packet (files, notification icons) travel
//! on their own TLS connection.
//!
//! The sender listens on a port from 1739 up and names it in the packet's
//! `payloadTransferInfo`; the receiver connects to it. Unlike the main link,
//! the roles are not inverted here: the sender, which accepted the TCP
//! connection, is the TLS server. Both sides pin the other's certificate, so
//! only a paired device can fetch or deliver a payload.

use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, Take};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::packet::Packet;
use crate::store::LocalIdentity;
use crate::tls::{self, PeerTrust};

/// Ports a payload may be offered on. The KDE Connect apps start at 1739
/// and stay inside the range the main link uses.
const PORT_RANGE: RangeInclusive<u16> = 1739..=1764;

/// How long either side waits for the other to show up, as on the peers.
const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) type PayloadStream = TlsStream<TcpStream>;

/// The paired device at the other end of a link, as payload connections
/// need to know it.
#[derive(Clone)]
pub(crate) struct Peer {
    pub address: IpAddr,
    /// The device's pinned certificate (DER).
    pub certificate: Arc<[u8]>,
    pub local: Arc<LocalIdentity>,
}

impl Peer {
    fn trust(&self) -> PeerTrust {
        PeerTrust::Pinned(self.certificate.to_vec())
    }
}

/// A payload the peer is offering with a packet.
pub(crate) struct Incoming {
    peer: Peer,
    port: u16,
    size: Option<u64>,
}

impl Incoming {
    /// The payload `packet` announces, if any. A size of -1 means the sender
    /// does not know it; the payload then ends when the connection closes.
    pub(crate) fn from_packet(packet: &Packet, peer: Peer) -> Option<Self> {
        let port = packet.payload_transfer_info.as_ref()?.get("port")?;
        let port = match port {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        };
        let port = u16::try_from(port?).ok()?;
        let size = match packet.payload_size? {
            0 => return None,
            size => u64::try_from(size).ok(),
        };
        Some(Self { peer, port, size })
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.size
    }

    /// Connects to the peer and returns a reader that yields exactly the
    /// payload's bytes (or everything until the peer closes, if the size is
    /// unknown).
    pub(crate) async fn open(self) -> io::Result<Take<PayloadStream>> {
        let stream = within(TcpStream::connect((self.peer.address, self.port))).await??;
        let config =
            tls::client_config(&self.peer.local, &self.peer.trust()).map_err(io::Error::other)?;
        let connector = TlsConnector::from(config);
        let stream = within(connector.connect(tls::server_name(), stream)).await??;
        Ok(TlsStream::Client(stream).take(self.size.unwrap_or(u64::MAX)))
    }
}

/// A payload we are offering: a listening port the peer will connect to.
pub(crate) struct Offer {
    listener: TcpListener,
    port: u16,
    peer: Peer,
}

impl Offer {
    /// Starts listening on the first free payload port.
    pub(crate) async fn bind(peer: Peer) -> io::Result<Self> {
        for port in PORT_RANGE {
            if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)).await {
                return Ok(Self {
                    listener,
                    port,
                    peer,
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "no free payload port",
        ))
    }

    /// Marks `packet` as carrying this payload of `size` bytes.
    pub(crate) fn attach(&self, packet: &mut Packet, size: u64) {
        let mut info = Map::new();
        info.insert("port".into(), self.port.into());
        packet.payload_size = Some(i64::try_from(size).unwrap_or(i64::MAX));
        packet.payload_transfer_info = Some(info);
    }

    /// Waits for the peer to fetch the payload and returns the stream to
    /// write it to. Connections from any other address are dropped.
    pub(crate) async fn accept(self) -> io::Result<PayloadStream> {
        let stream = within(async {
            loop {
                let (stream, from) = self.listener.accept().await?;
                if from.ip().to_canonical() == self.peer.address.to_canonical() {
                    return io::Result::Ok(stream);
                }
            }
        })
        .await??;
        let config =
            tls::server_config(&self.peer.local, &self.peer.trust()).map_err(io::Error::other)?;
        let stream = within(TlsAcceptor::from(config).accept(stream)).await??;
        Ok(TlsStream::Server(stream))
    }
}

async fn within<T>(future: impl Future<Output = T>) -> io::Result<T> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the peer did not connect"))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use tokio::io::AsyncWriteExt;

    use super::*;

    fn peer(local: &Arc<LocalIdentity>, other: &LocalIdentity) -> Peer {
        Peer {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            certificate: other.certificate_der().as_ref().into(),
            local: Arc::clone(local),
        }
    }

    fn identities() -> (Arc<LocalIdentity>, Arc<LocalIdentity>) {
        (
            Arc::new(LocalIdentity::generate().unwrap()),
            Arc::new(LocalIdentity::generate().unwrap()),
        )
    }

    #[tokio::test]
    async fn a_payload_reaches_the_paired_peer() {
        let (sender, receiver) = identities();
        let data = vec![7u8; 100_000];

        let offer = Offer::bind(peer(&sender, &receiver)).await.unwrap();
        let mut packet = Packet::new("kdeconnect.share.request", Map::new());
        offer.attach(&mut packet, data.len() as u64);
        let sent = data.clone();
        let sending = tokio::spawn(async move {
            let mut stream = offer.accept().await.unwrap();
            // More than was announced: the reader must stop at the size.
            stream.write_all(&sent).await.unwrap();
            stream.write_all(b"trailing").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        // The packet goes over the wire as a line and comes back parsed.
        let packet = Packet::parse(&packet.to_line()).unwrap();
        let incoming = Incoming::from_packet(&packet, peer(&receiver, &sender)).unwrap();
        assert_eq!(incoming.size(), Some(100_000));
        let mut received = Vec::new();
        incoming
            .open()
            .await
            .unwrap()
            .read_to_end(&mut received)
            .await
            .unwrap();
        assert_eq!(received, data);
        sending.await.unwrap();
    }

    #[tokio::test]
    async fn a_peer_with_another_certificate_cannot_fetch_a_payload() {
        let (sender, receiver) = identities();
        let stranger = Arc::new(LocalIdentity::generate().unwrap());

        let offer = Offer::bind(peer(&sender, &receiver)).await.unwrap();
        let mut packet = Packet::new("kdeconnect.share.request", Map::new());
        offer.attach(&mut packet, 4);
        let sending = tokio::spawn(async move { offer.accept().await.map(|_| ()) });

        let incoming = Incoming::from_packet(&packet, peer(&stranger, &sender)).unwrap();
        // TLS 1.3 reports a rejected client certificate only on first use.
        let fetched = async {
            let mut stream = incoming.open().await?;
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).await.map(|_| ())
        };
        assert!(fetched.await.is_err());
        assert!(sending.await.unwrap().is_err());
    }

    #[test]
    fn packets_without_a_usable_payload_offer_none() {
        let (local, other) = identities();
        let peer = peer(&local, &other);
        let plain = Packet::new("kdeconnect.ping", Map::new());
        assert!(Incoming::from_packet(&plain, peer.clone()).is_none());

        let mut info = Map::new();
        info.insert("port".into(), "1740".into());
        let mut packet = plain.clone();
        packet.payload_transfer_info = Some(info);
        packet.payload_size = Some(0);
        assert!(Incoming::from_packet(&packet, peer.clone()).is_none());

        packet.payload_size = Some(-1);
        let unknown = Incoming::from_packet(&packet, peer).unwrap();
        assert_eq!(unknown.size(), None);
    }
}
