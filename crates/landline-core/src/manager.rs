//! The device manager: one task that owns every device, link and pairing.
//!
//! Everything that changes state arrives as a [`Msg`] on one queue, so no
//! state is shared between tasks except what a handshake needs to look up
//! ([`Shared`]). Handshakes and socket reads run in their own tasks and
//! report back through the queue.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Map;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::discovery::{self, Announcement, Discovery, DiscoveryEvent};
use crate::identity::{self, DeviceType, Identity};
use crate::link::{self, Link, LinkReader, LinkWriter, TrustLookup};
use crate::packet::{Packet, TYPE_PAIR, TYPE_PING};
use crate::pairing::{self, Action, Pairing, PairingEvent, PairingFailure, TimerId};
use crate::store::{self, LocalIdentity, TrustStore, TrustedDevice};
use crate::{Config, CoreDelegate, Device, NetworkStatus};

/// Packet types this build handles. Peers never send a type we do not list,
/// so this grows only as plugins are implemented.
const CAPABILITIES: &[&str] = &[TYPE_PING];

/// The peers ignore a second identity from the same device within a second;
/// we do the same, which also absorbs a broadcast that reaches us twice.
const IDENTITY_INTERVAL: Duration = Duration::from_secs(1);

/// How often to announce again while a paired device is unreachable or no
/// device is connected at all. The protocol has no reconnect of its own: a
/// link comes back only when one side announces and the other connects, and
/// a phone with its screen off can miss a single broadcast.
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(15);

pub(crate) enum Msg {
    Start,
    Stop,
    NetworkChanged,
    Tick,
    AnnounceTo(IpAddr),
    Discovery(DiscoveryEvent),
    LinkUp(Box<Link>),
    Packet {
        device_id: String,
        link: u64,
        packet: Packet,
    },
    LinkDown {
        device_id: String,
        link: u64,
    },
    PairTimer {
        device_id: String,
        timer: TimerId,
    },
    Command {
        device_id: String,
        command: Command,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    RequestPair,
    AcceptPair,
    RejectPair,
    Unpair,
    Ping,
}

/// What handshake tasks read while the manager keeps running.
pub(crate) struct Shared {
    local: LocalIdentity,
    identity: Identity,
    /// Pinned certificate and protocol version of each paired device.
    pinned: RwLock<HashMap<String, (Vec<u8>, i64)>>,
    tx: UnboundedSender<Msg>,
}

impl TrustLookup for Shared {
    fn pinned(&self, device_id: &str) -> Option<(Vec<u8>, i64)> {
        self.pinned
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(device_id)
            .cloned()
    }
}

struct DeviceEntry {
    name: String,
    device_type: DeviceType,
    protocol_version: i64,
    /// The certificate the device presented on its current or last link, or
    /// the pinned one for a paired device that has not connected yet.
    certificate: Vec<u8>,
    /// `SubjectPublicKeyInfo` of `certificate`, for the verification key.
    public_key: Vec<u8>,
    pairing: Pairing,
    pair_timer: Option<JoinHandle<()>>,
    link: Option<LinkHandle>,
}

struct LinkHandle {
    id: u64,
    outgoing: UnboundedSender<Packet>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl LinkHandle {
    fn close(self) {
        self.reader.abort();
        self.writer.abort();
    }
}

struct Running {
    discovery: Discovery,
    tcp_port: u16,
    tasks: Vec<JoinHandle<()>>,
}

pub(crate) struct Manager {
    config: Config,
    shared: Arc<Shared>,
    trust: TrustStore,
    delegate: Arc<dyn CoreDelegate>,
    snapshot: Arc<Mutex<Vec<Device>>>,
    devices: HashMap<String, DeviceEntry>,
    running: Option<Running>,
    next_link: u64,
    last_identity: HashMap<String, Instant>,
    network_status: Option<NetworkStatus>,
}

impl Manager {
    pub(crate) fn new(
        config: Config,
        local: LocalIdentity,
        trust: TrustStore,
        delegate: Arc<dyn CoreDelegate>,
        snapshot: Arc<Mutex<Vec<Device>>>,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        let name = identity::sanitize_device_name(&config.device_name);
        let capabilities: Vec<String> = CAPABILITIES.iter().map(|&c| c.to_owned()).collect();
        let identity = Identity {
            device_id: local.device_id().to_owned(),
            device_name: if name.is_empty() { "Mac".into() } else { name },
            device_type: config.device_type,
            protocol_version: identity::PROTOCOL_VERSION,
            incoming_capabilities: capabilities.clone(),
            outgoing_capabilities: capabilities,
            tcp_port: None,
            target_device_id: None,
            target_protocol_version: None,
        };

        let mut devices = HashMap::new();
        let mut pinned = HashMap::new();
        for device in trust.iter() {
            let Ok(public_key) = store::subject_public_key_info(&device.certificate) else {
                warn!(device = %device.device_id, "ignoring a paired device with an unreadable certificate");
                continue;
            };
            let protocol_version = i64::from(device.protocol_version);
            pinned.insert(
                device.device_id.clone(),
                (device.certificate.clone(), protocol_version),
            );
            devices.insert(
                device.device_id.clone(),
                DeviceEntry {
                    name: device.name.clone(),
                    device_type: DeviceType::parse(&device.device_type),
                    protocol_version,
                    certificate: device.certificate.clone(),
                    public_key,
                    pairing: Pairing::new(protocol_version, true),
                    pair_timer: None,
                    link: None,
                },
            );
        }

        let manager = Self {
            config,
            shared: Arc::new(Shared {
                local,
                identity,
                pinned: RwLock::new(pinned),
                tx,
            }),
            trust,
            delegate,
            snapshot,
            devices,
            running: None,
            next_link: 0,
            last_identity: HashMap::new(),
            network_status: None,
        };
        // Paired devices are listed before anything connects.
        *manager
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = manager.device_list();
        manager
    }

    pub(crate) async fn run(mut self, mut rx: UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Start => self.start().await,
                Msg::Stop => self.stop(),
                Msg::NetworkChanged => self.network_changed().await,
                Msg::Tick => self.tick().await,
                Msg::AnnounceTo(address) => self.announce_to(address).await,
                Msg::Discovery(event) => self.discovery_event(event).await,
                Msg::LinkUp(link) => self.link_up(*link),
                Msg::Packet {
                    device_id,
                    link,
                    packet,
                } => self.packet_received(&device_id, link, packet),
                Msg::LinkDown { device_id, link } => self.link_down(&device_id, link),
                Msg::PairTimer { device_id, timer } => {
                    if let Some(entry) = self.devices.get_mut(&device_id) {
                        let actions = entry.pairing.timed_out(timer);
                        self.run_actions(&device_id, actions);
                    }
                }
                Msg::Command { device_id, command } => self.command(&device_id, command),
            }
        }
    }

    async fn start(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(listener) = bind_listener().await else {
            warn!("no free TCP port in the KDE Connect range; cannot accept connections");
            self.set_network_status(NetworkStatus::Offline);
            return;
        };
        let tcp_port = listener.local_addr().map(|a| a.port()).unwrap_or_default();

        let (events, mut event_rx) = mpsc::channel(64);
        let discovery = match Discovery::start(self.config.discovery.clone(), events) {
            Ok(discovery) => discovery,
            Err(error) => {
                warn!(%error, "discovery could not start");
                self.set_network_status(NetworkStatus::Offline);
                return;
            }
        };

        let mut tasks = Vec::new();
        let shared = Arc::clone(&self.shared);
        tasks.push(tokio::spawn(accept_loop(listener, shared)));

        let tx = self.shared.tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if tx.send(Msg::Discovery(event)).is_err() {
                    break;
                }
            }
        }));

        let tx = self.shared.tx.clone();
        tasks.push(tokio::spawn(async move {
            let mut ticks = tokio::time::interval(REANNOUNCE_INTERVAL);
            ticks.tick().await; // the first tick completes immediately
            loop {
                ticks.tick().await;
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        }));

        info!(tcp_port, "core started");
        self.running = Some(Running {
            discovery,
            tcp_port,
            tasks,
        });
        self.announce_mdns();
        self.broadcast().await;
    }

    fn stop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        for task in running.tasks {
            task.abort();
        }
        // Dropping discovery waits for its mDNS thread, so keep it off the
        // async workers.
        tokio::task::spawn_blocking(move || drop(running.discovery));

        let ids: Vec<String> = self.devices.keys().cloned().collect();
        for id in ids {
            self.drop_link(&id);
        }
        self.last_identity.clear();
        self.network_status = None;
        info!("core stopped");
        self.publish();
    }

    async fn network_changed(&mut self) {
        if let Some(running) = &self.running {
            running.discovery.refresh();
            self.announce_mdns();
            self.broadcast().await;
        }
    }

    async fn tick(&mut self) {
        let connected = self.devices.values().any(|entry| entry.link.is_some());
        let waiting = self
            .devices
            .values()
            .any(|entry| entry.pairing.is_paired() && entry.link.is_none());
        if waiting || !connected {
            self.broadcast().await;
        }
    }

    /// Our identity as the datagram peers expect, naming the TCP port they
    /// should connect back to.
    fn datagram(&self, tcp_port: u16) -> Vec<u8> {
        let mut identity = self.shared.identity.clone();
        identity.tcp_port = Some(tcp_port);
        identity.to_packet().to_line().into_bytes()
    }

    fn announce_mdns(&self) {
        let Some(running) = &self.running else {
            return;
        };
        let identity = &self.shared.identity;
        let announcement = Announcement {
            device_id: identity.device_id.clone(),
            device_name: identity.device_name.clone(),
            device_type: identity.device_type.as_str().to_owned(),
            protocol_version: u32::try_from(identity.protocol_version).unwrap_or_default(),
            tcp_port: running.tcp_port,
        };
        if let Err(error) = running.discovery.announce(announcement) {
            debug!(%error, "mDNS announcement failed");
        }
    }

    async fn broadcast(&mut self) {
        let Some(running) = &self.running else {
            return;
        };
        if !self.config.broadcast {
            return;
        }
        let report = running
            .discovery
            .broadcast(&self.datagram(running.tcp_port))
            .await;
        let status = if report.sent_any() {
            NetworkStatus::Ready
        } else if report.likely_local_network_denied() {
            NetworkStatus::Blocked
        } else {
            NetworkStatus::Offline
        };
        self.set_network_status(status);
    }

    async fn announce_to(&mut self, address: IpAddr) {
        let Some(running) = &self.running else {
            return;
        };
        let report = running
            .discovery
            .send_to(&self.datagram(running.tcp_port), address)
            .await;
        if !report.sent_any() {
            debug!(%address, "could not send our identity");
        }
    }

    fn set_network_status(&mut self, status: NetworkStatus) {
        if self.network_status != Some(status) {
            self.network_status = Some(status);
            self.delegate.network_status_changed(status);
        }
    }

    async fn discovery_event(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Datagram { data, from } => self.identity_received(&data, from),
            DiscoveryEvent::MdnsPeer(peer) => {
                let linked = peer
                    .device_id
                    .as_deref()
                    .and_then(|id| self.devices.get(id))
                    .is_some_and(|entry| entry.link.is_some());
                // Like the Android app, answer an mDNS sighting with our
                // identity rather than connecting: the device then connects
                // to us.
                if let (false, Some(&address)) = (linked, peer.addresses.first()) {
                    self.announce_to(address).await;
                }
            }
        }
    }

    /// A device announced itself over UDP: connect back to it.
    fn identity_received(&mut self, data: &[u8], from: SocketAddr) {
        let Ok(line) = std::str::from_utf8(data) else {
            return;
        };
        let peer = match link::parse_identity(line) {
            Ok(peer) => peer,
            Err(error) => {
                debug!(%from, %error, "ignoring a malformed identity");
                return;
            }
        };
        if peer.device_id == self.shared.identity.device_id {
            return;
        }
        let Some(port) = peer.tcp_port.filter(|&port| identity::is_link_port(port)) else {
            debug!(%from, "ignoring an identity without a usable TCP port");
            return;
        };

        let now = Instant::now();
        self.last_identity
            .retain(|_, seen| now.duration_since(*seen) < IDENTITY_INTERVAL);
        if self.last_identity.contains_key(&peer.device_id) {
            return;
        }
        self.last_identity.insert(peer.device_id.clone(), now);

        let shared = Arc::clone(&self.shared);
        let address = SocketAddr::new(from.ip(), port);
        tokio::spawn(async move {
            let link = link::connect(&shared.local, &shared.identity, &*shared, &peer, address);
            match link.await {
                Ok(link) => {
                    let _ = shared.tx.send(Msg::LinkUp(Box::new(link)));
                }
                Err(error) => debug!(%address, %error, "could not connect to a device"),
            }
        });
    }

    fn link_up(&mut self, link: Link) {
        if self.running.is_none() {
            return;
        }
        let Link {
            identity,
            certificate,
            address,
            reader,
            writer,
        } = link;
        let device_id = identity.device_id.clone();
        let Ok(public_key) = store::subject_public_key_info(&certificate) else {
            warn!(device = %device_id, "dropping a link whose certificate cannot be read");
            return;
        };

        // A device often connects twice in quick succession (it answers our
        // announcement while we answer its own), so a new link replaces the
        // old one without disturbing a pairing in progress. A different
        // certificate is a different key, though, and would change the
        // verification key under the user's eyes: that ends the attempt.
        if let Some(entry) = self.devices.get_mut(&device_id) {
            if entry.certificate != certificate {
                self.drop_link(&device_id);
            } else if let Some(old) = entry.link.take() {
                old.close();
            }
        }
        let entry = self
            .devices
            .entry(device_id.clone())
            .or_insert_with(|| DeviceEntry {
                name: identity.device_name.clone(),
                device_type: identity.device_type,
                protocol_version: identity.protocol_version,
                certificate: certificate.clone(),
                public_key: public_key.clone(),
                pairing: Pairing::new(identity.protocol_version, false),
                pair_timer: None,
                link: None,
            });
        entry.name = identity.device_name.clone();
        entry.device_type = identity.device_type;
        entry.protocol_version = identity.protocol_version;
        entry.certificate = certificate;
        entry.public_key = public_key;

        self.next_link += 1;
        let id = self.next_link;
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        entry.link = Some(LinkHandle {
            id,
            outgoing,
            reader: tokio::spawn(read_loop(
                reader,
                device_id.clone(),
                id,
                self.shared.tx.clone(),
            )),
            writer: tokio::spawn(write_loop(
                writer,
                outgoing_rx,
                device_id.clone(),
                id,
                self.shared.tx.clone(),
            )),
        });
        info!(device = %device_id, %address, "device connected");

        if entry.pairing.is_paired() {
            self.remember(&device_id);
        }
        self.publish();
    }

    fn packet_received(&mut self, device_id: &str, link: u64, packet: Packet) {
        let Some(entry) = self.devices.get_mut(device_id) else {
            return;
        };
        if entry.link.as_ref().map(|handle| handle.id) != Some(link) {
            return;
        }
        if packet.kind == TYPE_PAIR {
            let actions = entry.pairing.packet_received(&packet, unix_now());
            self.run_actions(device_id, actions);
            return;
        }
        if !entry.pairing.is_paired() {
            // The device believes it is paired with us. Telling it otherwise
            // is how the two sides get back in step.
            if let Some(handle) = &entry.link {
                let _ = handle.outgoing.send(pairing::unpair_packet());
            }
            return;
        }
        if packet.kind == TYPE_PING {
            self.delegate.ping_received(device_id);
        }
        self.delegate.packet_received(device_id, &packet);
    }

    fn link_down(&mut self, device_id: &str, link: u64) {
        let current = self
            .devices
            .get(device_id)
            .and_then(|entry| entry.link.as_ref())
            .map(|handle| handle.id);
        if current != Some(link) {
            return;
        }
        info!(device = %device_id, "device disconnected");
        self.drop_link(device_id);
        self.publish();
    }

    /// Closes the device's link, if any, and ends a pairing attempt that
    /// cannot finish without it.
    fn drop_link(&mut self, device_id: &str) {
        let Some(entry) = self.devices.get_mut(device_id) else {
            return;
        };
        let Some(handle) = entry.link.take() else {
            return;
        };
        handle.close();
        let actions = match entry.pairing.state() {
            pairing::PairState::Requested => entry.pairing.send_failed(),
            pairing::PairState::RequestedByPeer => entry.pairing.cancel(),
            pairing::PairState::NotPaired | pairing::PairState::Paired => Vec::new(),
        };
        self.run_actions(device_id, actions);
    }

    fn command(&mut self, device_id: &str, command: Command) {
        let Some(entry) = self.devices.get_mut(device_id) else {
            return;
        };
        let reachable = entry.link.is_some();
        let actions = match command {
            Command::RequestPair if !reachable => {
                self.delegate
                    .pairing_failed(device_id, PairingFailure::NotReachable);
                return;
            }
            Command::RequestPair => entry.pairing.request(unix_now()),
            Command::AcceptPair => entry.pairing.accept(),
            Command::RejectPair => entry.pairing.cancel(),
            Command::Unpair => entry.pairing.unpair(reachable),
            Command::Ping => {
                if let (true, Some(handle)) = (entry.pairing.is_paired(), &entry.link) {
                    let _ = handle.outgoing.send(Packet::new(TYPE_PING, Map::new()));
                }
                return;
            }
        };
        self.run_actions(device_id, actions);
    }

    /// Carries out what the pairing state machine asked for, then publishes
    /// the device list.
    fn run_actions(&mut self, device_id: &str, actions: Vec<Action>) {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            let Some(entry) = self.devices.get_mut(device_id) else {
                break;
            };
            match action {
                Action::Send(packet) => {
                    let sent = entry
                        .link
                        .as_ref()
                        .is_some_and(|handle| handle.outgoing.send(packet).is_ok());
                    if !sent {
                        queue.extend(entry.pairing.send_failed());
                    }
                }
                Action::StartTimer { timer, after } => {
                    if let Some(old) = entry.pair_timer.take() {
                        old.abort();
                    }
                    let tx = self.shared.tx.clone();
                    let device_id = device_id.to_owned();
                    entry.pair_timer = Some(tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = tx.send(Msg::PairTimer { device_id, timer });
                    }));
                }
                Action::CancelTimer => {
                    if let Some(timer) = entry.pair_timer.take() {
                        timer.abort();
                    }
                }
                Action::Event(PairingEvent::Paired) => {
                    info!(device = %device_id, "paired");
                    self.remember(device_id);
                }
                Action::Event(PairingEvent::Unpaired) => {
                    info!(device = %device_id, "unpaired");
                    self.forget(device_id);
                }
                Action::Event(PairingEvent::Failed(reason)) => {
                    // The user already knows about their own cancellation.
                    if reason != PairingFailure::CancelledByUser {
                        self.publish();
                        self.delegate.pairing_failed(device_id, reason);
                    }
                }
                Action::Event(PairingEvent::IncomingRequest) => {}
            }
        }
        self.publish();
    }

    /// Pins the device's current certificate and records it as paired.
    fn remember(&mut self, device_id: &str) {
        let Some(entry) = self.devices.get(device_id) else {
            return;
        };
        let record = TrustedDevice {
            device_id: device_id.to_owned(),
            certificate: entry.certificate.clone(),
            name: entry.name.clone(),
            device_type: entry.device_type.as_str().to_owned(),
            protocol_version: u32::try_from(entry.protocol_version).unwrap_or_default(),
        };
        self.shared
            .pinned
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                device_id.to_owned(),
                (record.certificate.clone(), entry.protocol_version),
            );
        if self.trust.get(device_id) != Some(&record)
            && let Err(error) = self.trust.insert(record)
        {
            warn!(device = %device_id, %error, "could not save the pairing");
        }
    }

    fn forget(&mut self, device_id: &str) {
        self.shared
            .pinned
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(device_id);
        if let Err(error) = self.trust.remove(device_id) {
            warn!(device = %device_id, %error, "could not remove the pairing");
        }
    }

    fn device_list(&self) -> Vec<Device> {
        let our_key = self.shared.local.public_key_der();
        let mut devices: Vec<Device> = self
            .devices
            .iter()
            .map(|(id, entry)| Device {
                id: id.clone(),
                name: entry.name.clone(),
                device_type: entry.device_type,
                reachable: entry.link.is_some(),
                pair_state: entry.pairing.state(),
                verification_key: entry.pairing.verification_key(our_key, &entry.public_key),
            })
            .collect();
        devices.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        devices
    }

    /// Drops devices that are neither paired nor connected, and tells the
    /// delegate if the list changed.
    fn publish(&mut self) {
        self.devices
            .retain(|_, entry| entry.link.is_some() || entry.pairing.is_paired());
        let devices = self.device_list();
        let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        if *snapshot != devices {
            snapshot.clone_from(&devices);
            drop(snapshot);
            self.delegate.devices_changed(devices);
        }
    }
}

/// Binds the first free port in the range peers will connect to.
async fn bind_listener() -> Option<TcpListener> {
    for port in identity::TCP_PORT_RANGE {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            return Some(listener);
        }
    }
    None
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, address) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                debug!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !discovery::is_private_address(address.ip()) {
            debug!(%address, "refusing a connection from outside the local network");
            continue;
        }
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let link = link::accept(&shared.local, &shared.identity, &*shared, stream, address);
            match link.await {
                Ok(link) => {
                    let _ = shared.tx.send(Msg::LinkUp(Box::new(link)));
                }
                Err(error) => debug!(%address, %error, "incoming connection failed"),
            }
        });
    }
}

async fn read_loop(mut reader: LinkReader, device_id: String, link: u64, tx: UnboundedSender<Msg>) {
    loop {
        let line = match link::read_line(&mut reader, link::MAX_PACKET_SIZE).await {
            Ok(line) => line,
            Err(error) => {
                debug!(device = %device_id, %error, "link closed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match Packet::parse(&line) {
            Ok(packet) => {
                let msg = Msg::Packet {
                    device_id: device_id.clone(),
                    link,
                    packet,
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Err(error) => debug!(device = %device_id, %error, "ignoring a malformed packet"),
        }
    }
    let _ = tx.send(Msg::LinkDown { device_id, link });
}

async fn write_loop(
    mut writer: LinkWriter,
    mut outgoing: UnboundedReceiver<Packet>,
    device_id: String,
    link: u64,
    tx: UnboundedSender<Msg>,
) {
    while let Some(packet) = outgoing.recv().await {
        if let Err(error) = link::write_packet(&mut writer, &packet).await {
            debug!(device = %device_id, %error, "write failed");
            let _ = tx.send(Msg::LinkDown { device_id, link });
            return;
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
