//! The device manager: one task that owns every device, link and pairing.
//!
//! Everything that changes state arrives as a [`Msg`] on one queue, so no
//! state is shared between tasks except what a handshake needs to look up
//! ([`Shared`]). Handshakes and socket reads run in their own tasks and
//! report back through the queue.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::discovery::{self, Announcement, Discovery, DiscoveryEvent};
use crate::identity::{self, DeviceType, Identity};
use crate::link::{self, Link, LinkReader, LinkWriter, TrustLookup};
use crate::packet::{Packet, TYPE_PAIR};
use crate::pairing::{self, Action, Pairing, PairingEvent, PairingFailure, TimerId};
use crate::payload::Peer;
use crate::plugins::{
    self, Context, DeviceStatus, LinkContext, PacketSender, Plugin, REGISTRY, RecentOtps,
};
use crate::store::{self, LocalIdentity, TrustStore, TrustedDevice};
use crate::{Config, CoreDelegate, CoreError, Device, NetworkStatus};

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
    Plugin {
        device_id: String,
        command: plugins::Command,
    },
    /// Whether an attempt to reach a device on the local network was
    /// refused outright (`true`) or went out (`false`).
    Outbound {
        refused: bool,
    },
    /// Offer this text to every usable device.
    Clipboard(String),
    SetDownloadDir(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    RequestPair,
    AcceptPair,
    RejectPair,
    Unpair,
}

/// What handshake tasks read while the manager keeps running.
pub(crate) struct Shared {
    local: Arc<LocalIdentity>,
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
    /// One per entry of [`REGISTRY`], in the same order.
    plugins: Vec<Box<dyn Plugin>>,
    /// Whether the plugins have been told the device is usable (paired and
    /// connected).
    usable: bool,
    status: DeviceStatus,
    recent_otps: RecentOtps,
    /// Where the device was last reached, for announcing straight to it.
    last_address: Option<IpAddr>,
}

impl DeviceEntry {
    fn new(
        name: String,
        device_type: DeviceType,
        protocol_version: i64,
        certificate: Vec<u8>,
        public_key: Vec<u8>,
        paired: bool,
    ) -> Self {
        Self {
            name,
            device_type,
            protocol_version,
            certificate,
            public_key,
            pairing: Pairing::new(protocol_version, paired),
            pair_timer: None,
            link: None,
            plugins: REGISTRY.iter().map(|plugin| (plugin.create)()).collect(),
            usable: false,
            status: DeviceStatus::default(),
            recent_otps: RecentOtps::default(),
            last_address: None,
        }
    }
}

struct LinkHandle {
    id: u64,
    address: IpAddr,
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
    /// What the last broadcast said about the network.
    broadcast_status: Option<NetworkStatus>,
    /// How many attempts in a row to reach a device on the local network
    /// the system has refused outright. On macOS that is the Local Network
    /// permission; broadcasts still go out and devices can still connect to
    /// us, so only this shows it.
    outbound_refusals: u32,
    download_dir: PathBuf,
    cache_dir: PathBuf,
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
        // Peers never send a packet type we do not list, so the identity
        // carries exactly what the plugins handle.
        let capabilities = |pick: fn(&plugins::Registration) -> &'static [&'static str]| {
            REGISTRY
                .iter()
                .flat_map(|plugin| pick(plugin).iter().map(|&kind| kind.to_owned()))
                .collect::<Vec<String>>()
        };
        let identity = Identity {
            device_id: local.device_id().to_owned(),
            device_name: if name.is_empty() { "Mac".into() } else { name },
            device_type: config.device_type,
            protocol_version: identity::PROTOCOL_VERSION,
            incoming_capabilities: capabilities(|plugin| plugin.incoming),
            outgoing_capabilities: capabilities(|plugin| plugin.outgoing),
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
            let mut entry = DeviceEntry::new(
                device.name.clone(),
                DeviceType::parse(&device.device_type),
                protocol_version,
                device.certificate.clone(),
                public_key,
                true,
            );
            entry.last_address = device.last_address;
            devices.insert(device.device_id.clone(), entry);
        }

        let manager = Self {
            download_dir: config.download_dir.clone(),
            cache_dir: config.data_dir.join("cache"),
            config,
            shared: Arc::new(Shared {
                local: Arc::new(local),
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
            broadcast_status: None,
            outbound_refusals: 0,
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
                Msg::Plugin { device_id, command } => self.plugin_command(&device_id, command),
                Msg::Clipboard(text) => {
                    let usable: Vec<String> = self
                        .devices
                        .iter()
                        .filter(|(_, entry)| entry.usable)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for device_id in usable {
                        let command = plugins::Command::SendClipboard { text: text.clone() };
                        self.plugin_command(&device_id, command);
                    }
                }
                Msg::SetDownloadDir(path) => self.download_dir = path,
                Msg::Outbound { refused } => self.outbound_result(refused),
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
        self.broadcast_status = None;
        self.outbound_refusals = 0;
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

    /// Sends our identity straight to each paired device that is not
    /// connected, at the address it last had.
    async fn announce_to_known(&mut self) {
        let addresses: Vec<IpAddr> = self
            .devices
            .values()
            .filter(|entry| entry.pairing.is_paired() && entry.link.is_none())
            .filter_map(|entry| entry.last_address)
            .collect();
        for address in addresses {
            self.announce_to(address).await;
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
        self.broadcast_status = Some(if report.sent_any() {
            NetworkStatus::Ready
        } else if report.likely_local_network_denied() {
            NetworkStatus::Blocked
        } else {
            NetworkStatus::Offline
        });
        self.update_network_status();
        self.announce_to_known().await;
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
        // A datagram to an absent host still leaves, or fails as "host is
        // down"; only a refusal by the system fails as "no route".
        self.outbound_result(!report.sent_any() && report.likely_local_network_denied());
    }

    fn outbound_result(&mut self, refused: bool) {
        self.outbound_refusals = if refused {
            self.outbound_refusals.saturating_add(1)
        } else {
            0
        };
        self.update_network_status();
    }

    fn update_network_status(&mut self) {
        if self.running.is_none() {
            return;
        }
        // One refusal is not a verdict: a paired phone that has gone to
        // sleep fails the same way for a moment (seen as one "no route to
        // host", then "host is down", then sends that leave normally),
        // while a missing permission refuses every attempt.
        if self.outbound_refusals >= 3 {
            self.set_network_status(NetworkStatus::Blocked);
        } else if let Some(status) = self.broadcast_status {
            self.set_network_status(status);
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
                    let _ = shared.tx.send(Msg::Outbound { refused: false });
                    let _ = shared.tx.send(Msg::LinkUp(Box::new(link)));
                }
                Err(error) => {
                    debug!(%address, %error, "could not connect to a device");
                    if error.is_refused_by_system() {
                        let _ = shared.tx.send(Msg::Outbound { refused: true });
                    }
                }
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
                // Plugins hold senders for the old link; let them start over.
                self.sync_plugins(&device_id);
            }
        }
        let entry = self.devices.entry(device_id.clone()).or_insert_with(|| {
            DeviceEntry::new(
                identity.device_name.clone(),
                identity.device_type,
                identity.protocol_version,
                certificate.clone(),
                public_key.clone(),
                false,
            )
        });
        entry.name = identity.device_name.clone();
        entry.device_type = identity.device_type;
        entry.protocol_version = identity.protocol_version;
        entry.certificate = certificate;
        entry.public_key = public_key;
        entry.last_address = Some(address.ip());

        self.next_link += 1;
        let id = self.next_link;
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        entry.link = Some(LinkHandle {
            id,
            address: address.ip(),
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
        self.sync_plugins(&device_id);
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
        self.delegate.packet_received(device_id, &packet);
        let handler = REGISTRY
            .iter()
            .position(|plugin| plugin.incoming.contains(&packet.kind.as_str()));
        if let Some(index) = handler {
            self.plugins(device_id, |plugins, ctx| {
                plugins[index].packet(&packet, ctx)
            });
        }
    }

    /// Runs `call` with the device's plugins and a context for them, then
    /// publishes the device list if a plugin changed the device's status.
    fn plugins(
        &mut self,
        device_id: &str,
        call: impl FnOnce(&mut [Box<dyn Plugin>], &mut Context<'_>),
    ) {
        let Some(entry) = self.devices.get_mut(device_id) else {
            return;
        };
        let mut plugins = std::mem::take(&mut entry.plugins);
        let before = entry.status.clone();
        let link = entry
            .link
            .as_ref()
            .filter(|_| entry.usable)
            .map(|handle| LinkContext {
                sender: PacketSender(handle.outgoing.clone()),
                peer: Peer {
                    address: handle.address,
                    certificate: entry.certificate.as_slice().into(),
                    local: Arc::clone(&self.shared.local),
                },
            });
        let mut ctx = Context {
            device_id,
            delegate: &self.delegate,
            download_dir: &self.download_dir,
            cache_dir: &self.cache_dir,
            link,
            status: &mut entry.status,
            recent_otps: &mut entry.recent_otps,
        };
        call(&mut plugins, &mut ctx);
        entry.plugins = plugins;
        if entry.status != before {
            self.publish();
        }
    }

    /// Tells the device's plugins when it starts or stops being usable
    /// (paired and connected).
    fn sync_plugins(&mut self, device_id: &str) {
        let Some(entry) = self.devices.get_mut(device_id) else {
            return;
        };
        let usable = entry.pairing.is_paired() && entry.link.is_some();
        if usable == entry.usable {
            return;
        }
        entry.usable = usable;
        self.plugins(device_id, |plugins, ctx| {
            for plugin in plugins {
                if usable {
                    plugin.connected(ctx);
                } else {
                    plugin.disconnected(ctx);
                }
            }
        });
        if !usable && let Some(entry) = self.devices.get_mut(device_id) {
            // What the device last reported says nothing about it now.
            entry.status = DeviceStatus::default();
        }
    }

    fn plugin_command(&mut self, device_id: &str, command: plugins::Command) {
        // A transfer ID does not say which plugin runs the transfer, so
        // both that have transfers hear about a cancellation.
        if let plugins::Command::CancelTransfer { id } = command {
            for name in [
                plugins::share::REGISTRATION.name,
                plugins::sftp::REGISTRATION.name,
            ] {
                let handler = REGISTRY.iter().position(|plugin| plugin.name == name);
                if let Some(index) = handler {
                    self.plugins(device_id, |plugins, ctx| {
                        plugins[index].command(plugins::Command::CancelTransfer { id }, ctx);
                    });
                }
            }
            return;
        }
        let usable = self
            .devices
            .get(device_id)
            .is_some_and(|entry| entry.usable);
        let handler = REGISTRY
            .iter()
            .position(|plugin| plugin.name == command.plugin());
        match (usable, handler) {
            (true, Some(index)) => {
                self.plugins(device_id, |plugins, ctx| {
                    plugins[index].command(command, ctx)
                });
            }
            // A storage request carries a reply channel someone is awaiting.
            _ => {
                if let plugins::Command::Storage(request) = command {
                    request.fail(CoreError::NotReachable);
                }
            }
        }
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
        self.sync_plugins(device_id);
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
            last_address: entry.last_address,
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
                battery: entry.status.battery,
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
