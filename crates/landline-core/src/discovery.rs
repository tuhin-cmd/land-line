//! LAN discovery: UDP identity datagrams on port 1716 and mDNS
//! (`_kdeconnect._udp`).
//!
//! This module only moves bytes and addresses. It does not parse identity
//! packets, open TCP connections, or rate-limit peers; the link layer does
//! that with what arrives as [`DiscoveryEvent`]s.
//!
//! How KDE Connect peers find each other:
//!
//! - Each device sends its identity packet as one UDP datagram to port 1716,
//!   by broadcast or unicast. A device that receives one connects back over
//!   TCP to the sender's address. [`Discovery::broadcast`] and
//!   [`Discovery::send_to`] send ours; [`DiscoveryEvent::Datagram`] delivers
//!   theirs.
//! - Each device also advertises an mDNS service whose instance name is its
//!   device ID. A device that resolves one sends its UDP identity to that
//!   address, rather than connecting directly. [`Discovery::announce`]
//!   advertises us; [`DiscoveryEvent::MdnsPeer`] reports a resolved peer, and
//!   the caller answers with [`Discovery::send_to`].
//!
//! Our own broadcasts come back to our own listener. The listener cannot tell
//! them apart without parsing, so the caller drops identities that carry our
//! own device ID, as the KDE Connect apps do.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

/// The UDP port KDE Connect devices listen on and send identities to.
pub const DEFAULT_UDP_PORT: u16 = 1716;

/// The standard mDNS port.
pub const DEFAULT_MDNS_PORT: u16 = 5353;

/// The mDNS service type KDE Connect advertises, in the form `mdns-sd` wants.
pub const SERVICE_TYPE: &str = "_kdeconnect._udp.local.";

/// Large enough for any UDP datagram over IPv4, so nothing is truncated.
/// Identity packets are 1 to 3 KB in practice.
const RECV_BUFFER: usize = 65_536;

/// One TXT string (key, `=`, value) must fit in 255 bytes.
const TXT_STRING_MAX: usize = 255;

/// How long dropping [`Discovery`] waits for the mDNS daemon thread to exit.
const MDNS_SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// What [`Discovery`] listens on and sends to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryConfig {
    /// UDP port to receive identity datagrams on. `0` picks a free port,
    /// which tests use; [`ListenerState::Listening`] reports the one chosen.
    pub listen_port: u16,
    /// UDP port that [`Discovery::broadcast`] and [`Discovery::send_to`]
    /// send to.
    pub send_port: u16,
    /// Whether to run mDNS at all. When `false`, nothing is browsed and
    /// [`Discovery::announce`] fails with [`DiscoveryError::MdnsDisabled`].
    pub mdns: bool,
    /// Port the mDNS daemon uses. Peers only see us on
    /// [`DEFAULT_MDNS_PORT`]; other values keep tests off the real network
    /// service.
    pub mdns_port: u16,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_UDP_PORT,
            send_port: DEFAULT_UDP_PORT,
            mdns: true,
            mdns_port: DEFAULT_MDNS_PORT,
        }
    }
}

/// Something discovery saw on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A UDP datagram from a private address, normally a peer's identity
    /// packet. `data` is exactly what arrived, unparsed. Datagrams from
    /// non-private addresses are dropped before this point.
    Datagram { from: SocketAddr, data: Vec<u8> },
    /// A peer resolved over mDNS. It may be reported again whenever its
    /// records change or are re-resolved; the caller deduplicates.
    MdnsPeer(MdnsPeer),
}

/// A KDE Connect service resolved over mDNS, other than our own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsPeer {
    /// The peer's device ID: the `id` TXT record if present, otherwise the
    /// instance name when it has the shape of a device ID. `None` when
    /// neither is usable (for example after an mDNS rename such as `x (2)`).
    pub device_id: Option<String>,
    /// The mDNS instance name, which KDE Connect sets to the device ID.
    pub instance: String,
    /// The `name` TXT record, if present.
    pub device_name: Option<String>,
    /// The `type` TXT record (`phone`, `desktop`, ...), if present. The
    /// desktop app does not send it.
    pub device_type: Option<String>,
    /// The `protocol` TXT record, if present and numeric.
    pub protocol_version: Option<u32>,
    /// Private addresses of the peer, IPv4 first, never empty. IPv6
    /// link-local addresses are left out because they are unusable without
    /// the interface they were seen on.
    pub addresses: Vec<IpAddr>,
    /// The service port, which KDE Connect sets to its TCP port.
    pub port: u16,
}

/// Whether the UDP listener could bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerState {
    /// Receiving datagrams on `port`.
    Listening { port: u16 },
    /// The port could not be bound (for example another program holds it
    /// without `SO_REUSEPORT`). Sending still works; peers can still find us
    /// through our broadcasts and mDNS, but we hear no broadcasts.
    /// [`Discovery::refresh`] tries to bind again.
    SendOnly { error: IoFailure },
}

/// An I/O error in a form that can be cloned, compared, and shown to a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFailure {
    pub kind: io::ErrorKind,
    /// The raw `errno`, when there was one (65 is `EHOSTUNREACH` on macOS).
    pub os_code: Option<i32>,
    pub message: String,
}

impl From<&io::Error> for IoFailure {
    fn from(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            os_code: error.raw_os_error(),
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for IoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Where one send in a [`SendReport`] went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// 255.255.255.255, which macOS sends out of the default interface only.
    LimitedBroadcast,
    /// The broadcast address of one interface's subnet.
    DirectedBroadcast { interface: String },
    /// A single host.
    Unicast,
}

/// One datagram send and how it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendAttempt {
    pub target: SendTarget,
    pub to: SocketAddr,
    pub result: Result<(), IoFailure>,
}

/// The outcome of [`Discovery::broadcast`] or [`Discovery::send_to`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendReport {
    pub attempts: Vec<SendAttempt>,
}

impl SendReport {
    /// Whether at least one send was accepted by the OS. That does not mean
    /// anyone received it.
    pub fn sent_any(&self) -> bool {
        self.attempts.iter().any(|attempt| attempt.result.is_ok())
    }

    /// The failed sends.
    pub fn failures(&self) -> impl Iterator<Item = (&SendAttempt, &IoFailure)> {
        self.attempts
            .iter()
            .filter_map(|attempt| attempt.result.as_ref().err().map(|e| (attempt, e)))
    }

    /// A heuristic for "macOS Local Network privacy is blocking us": every
    /// send failed, and each with "No route to host" or "Permission denied".
    ///
    /// For a broadcast it also requires a directed broadcast attempt, since
    /// with no LAN interface at all the same errors just mean "no network".
    /// The OS gives no direct signal, so treat this as a hint for the UI.
    pub fn likely_local_network_denied(&self) -> bool {
        let is_broadcast = self
            .attempts
            .iter()
            .any(|a| a.target == SendTarget::LimitedBroadcast);
        let has_lan = self
            .attempts
            .iter()
            .any(|a| matches!(a.target, SendTarget::DirectedBroadcast { .. }));
        !self.attempts.is_empty()
            && (has_lan || !is_broadcast)
            && self.attempts.iter().all(|a| {
                matches!(
                    &a.result,
                    Err(IoFailure {
                        kind: io::ErrorKind::HostUnreachable | io::ErrorKind::PermissionDenied,
                        ..
                    })
                )
            })
    }
}

/// What we advertise over mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    /// Our device ID. Becomes the instance name and the `id` TXT record, and
    /// is filtered out of browse results.
    pub device_id: String,
    /// Shown by peers before they connect. Truncated to fit a TXT record.
    pub device_name: String,
    /// `phone`, `tablet`, `desktop`, `laptop`, or `tv`.
    pub device_type: String,
    pub protocol_version: u32,
    /// Our TCP listening port, advertised as the service port.
    pub tcp_port: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("discovery must be started from inside a tokio runtime")]
    NoRuntime,
    #[error("mDNS is disabled in the discovery config")]
    MdnsDisabled,
    #[error("mDNS error: {0}")]
    Mdns(String),
}

/// UDP and mDNS discovery. Events go to the channel given to
/// [`Discovery::start`].
///
/// Methods take `&self`, so one instance can be shared behind an `Arc`, and
/// the synchronous ones can be called from any thread. Dropping it stops
/// every task it started, unregisters the mDNS service, and shuts the mDNS
/// daemon down. That blocks while the daemon thread exits, normally for
/// milliseconds and never for more than a second, so async callers may want
/// to drop it from `spawn_blocking`.
pub struct Discovery {
    config: DiscoveryConfig,
    events: mpsc::Sender<DiscoveryEvent>,
    runtime: Handle,
    own_id: Arc<RwLock<Option<String>>>,
    state: Mutex<State>,
}

struct State {
    listener: ListenerState,
    listener_task: Option<JoinHandle<()>>,
    mdns: Option<Mdns>,
    /// Kept even when registering fails, so [`Discovery::refresh`] retries.
    announcement: Option<Announcement>,
}

struct Mdns {
    daemon: ServiceDaemon,
    browse_task: Option<JoinHandle<()>>,
    /// Full name of the registered service, for unregistering.
    registered: Option<String>,
}

impl Discovery {
    /// Binds the UDP listener and, if enabled, starts the mDNS daemon and
    /// browsing. Must be called from inside a tokio runtime, whose tasks it
    /// uses from then on.
    ///
    /// Neither a failed bind nor a failed mDNS start is an error: discovery
    /// keeps going without them, and [`listener_state`](Self::listener_state)
    /// and [`mdns_running`](Self::mdns_running) say what is missing.
    pub fn start(
        config: DiscoveryConfig,
        events: mpsc::Sender<DiscoveryEvent>,
    ) -> Result<Self, DiscoveryError> {
        let runtime = Handle::try_current().map_err(|_| DiscoveryError::NoRuntime)?;
        let discovery = Self {
            config,
            events,
            runtime,
            own_id: Arc::new(RwLock::new(None)),
            state: Mutex::new(State {
                listener: ListenerState::Listening { port: 0 },
                listener_task: None,
                mdns: None,
                announcement: None,
            }),
        };
        {
            let mut state = discovery.lock();
            discovery.start_listener(&mut state);
            if discovery.config.mdns {
                discovery.start_mdns(&mut state);
            }
        }
        debug!(config = ?discovery.config, "discovery started");
        Ok(discovery)
    }

    /// Whether the UDP listener is receiving, and on which port.
    pub fn listener_state(&self) -> ListenerState {
        self.lock().listener.clone()
    }

    /// Whether the mDNS daemon is running (it is not when disabled in the
    /// config or when it failed to start).
    pub fn mdns_running(&self) -> bool {
        self.lock().mdns.is_some()
    }

    /// Sets our device ID so that browsing skips our own service. Also set by
    /// [`announce`](Self::announce).
    pub fn set_own_device_id(&self, device_id: impl Into<String>) {
        *self.own_id.write().unwrap_or_else(PoisonError::into_inner) = Some(device_id.into());
    }

    /// Advertises us over mDNS, replacing any earlier announcement.
    ///
    /// The announcement is remembered even if registering fails, and
    /// [`refresh`](Self::refresh) registers it again.
    pub fn announce(&self, announcement: Announcement) -> Result<(), DiscoveryError> {
        if !self.config.mdns {
            return Err(DiscoveryError::MdnsDisabled);
        }
        self.set_own_device_id(announcement.device_id.clone());
        let mut state = self.lock();
        state.announcement = Some(announcement);
        let State {
            mdns, announcement, ..
        } = &mut *state;
        let (Some(mdns), Some(announcement)) = (mdns.as_mut(), announcement.as_ref()) else {
            return Err(DiscoveryError::Mdns(
                "the mDNS daemon is not running".into(),
            ));
        };
        mdns.register(announcement)
    }

    /// Withdraws our mDNS advertisement, if any.
    pub fn stop_announcing(&self) {
        let mut state = self.lock();
        state.announcement = None;
        if let Some(mdns) = state.mdns.as_mut() {
            mdns.unregister();
        }
    }

    /// Sends `datagram` to 255.255.255.255 and to the broadcast address of
    /// every up, non-loopback IPv4 interface with a private address, on the
    /// configured send port.
    ///
    /// The directed broadcasts are needed because macOS sends the limited
    /// broadcast out of the default interface only.
    pub async fn broadcast(&self, datagram: &[u8]) -> SendReport {
        let port = self.config.send_port;
        let mut targets = vec![(
            SendTarget::LimitedBroadcast,
            SocketAddr::from((Ipv4Addr::BROADCAST, port)),
        )];
        for (interface, address) in directed_broadcasts() {
            let to = SocketAddr::from((address, port));
            if targets.iter().all(|(_, seen)| *seen != to) {
                targets.push((SendTarget::DirectedBroadcast { interface }, to));
            }
        }
        let report = send_all(datagram, targets).await;
        log_report("broadcast", &report);
        report
    }

    /// Sends `datagram` to `ip` on the configured send port.
    pub async fn send_to(&self, datagram: &[u8], ip: IpAddr) -> SendReport {
        let to = SocketAddr::new(ip.to_canonical(), self.config.send_port);
        let report = send_all(datagram, vec![(SendTarget::Unicast, to)]).await;
        log_report("unicast", &report);
        report
    }

    /// Catches up with a network change (Wi-Fi toggled, wake from sleep, new
    /// address): binds the UDP listener again if it was send-only, restarts
    /// mDNS browsing, and registers the announcement again. Interfaces for
    /// broadcasts are enumerated on every send, so they need no refresh.
    ///
    /// Returns the listener state afterwards.
    pub fn refresh(&self) -> ListenerState {
        let mut state = self.lock();
        if matches!(state.listener, ListenerState::SendOnly { .. }) {
            self.start_listener(&mut state);
        }
        if self.config.mdns {
            let healthy = state.mdns.as_mut().is_some_and(|mdns| {
                mdns.restart_browse(&self.runtime, &self.events, &self.own_id)
                    .is_ok()
            });
            if !healthy {
                // A daemon that rejects commands has exited; build a new one.
                if let Some(mdns) = state.mdns.take() {
                    mdns.shut_down();
                }
                self.start_mdns(&mut state);
            }
            let State {
                mdns, announcement, ..
            } = &mut *state;
            if let (Some(mdns), Some(announcement)) = (mdns.as_mut(), announcement.as_ref()) {
                if let Err(error) = mdns.register(announcement) {
                    warn!(%error, "could not register the mDNS service again");
                }
            }
        }
        debug!(listener = ?state.listener, mdns = state.mdns.is_some(), "discovery refreshed");
        state.listener.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start_listener(&self, state: &mut State) {
        if let Some(task) = state.listener_task.take() {
            task.abort();
        }
        let _runtime = self.runtime.enter();
        match bind_listener(self.config.listen_port) {
            Ok((socket, port)) => {
                debug!(port, "UDP listener bound");
                state.listener = ListenerState::Listening { port };
                state.listener_task = Some(self.runtime.spawn(listen(socket, self.events.clone())));
            }
            Err(error) => {
                warn!(
                    port = self.config.listen_port,
                    %error,
                    "could not bind the UDP listener; continuing send-only"
                );
                state.listener = ListenerState::SendOnly {
                    error: IoFailure::from(&error),
                };
            }
        }
    }

    fn start_mdns(&self, state: &mut State) {
        let daemon = match ServiceDaemon::new_with_port(self.config.mdns_port) {
            Ok(daemon) => daemon,
            Err(error) => {
                warn!(%error, "could not start the mDNS daemon");
                return;
            }
        };
        let mut mdns = Mdns {
            daemon,
            browse_task: None,
            registered: None,
        };
        if let Err(error) = mdns.restart_browse(&self.runtime, &self.events, &self.own_id) {
            warn!(%error, "could not start mDNS browsing");
        }
        if let Some(announcement) = &state.announcement {
            if let Err(error) = mdns.register(announcement) {
                warn!(%error, "could not register the mDNS service");
            }
        }
        debug!(port = self.config.mdns_port, "mDNS daemon started");
        state.mdns = Some(mdns);
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let mut state = self.lock();
        if let Some(task) = state.listener_task.take() {
            task.abort();
        }
        if let Some(mdns) = state.mdns.take() {
            mdns.shut_down();
        }
        debug!("discovery stopped");
    }
}

impl Mdns {
    fn restart_browse(
        &mut self,
        runtime: &Handle,
        events: &mpsc::Sender<DiscoveryEvent>,
        own_id: &Arc<RwLock<Option<String>>>,
    ) -> Result<(), mdns_sd::Error> {
        if let Some(task) = self.browse_task.take() {
            task.abort();
            // Ends the old subscription inside the daemon, too.
            let _ = self.daemon.stop_browse(SERVICE_TYPE);
        }
        let receiver = self.daemon.browse(SERVICE_TYPE)?;
        self.browse_task =
            Some(runtime.spawn(browse(receiver, events.clone(), Arc::clone(own_id))));
        Ok(())
    }

    fn register(&mut self, announcement: &Announcement) -> Result<(), DiscoveryError> {
        let info = service_info(announcement).map_err(|e| DiscoveryError::Mdns(e.to_string()))?;
        let fullname = info.get_fullname().to_owned();
        if self.registered.as_ref().is_some_and(|old| *old != fullname) {
            self.unregister();
        }
        self.daemon
            .register(info)
            .map_err(|e| DiscoveryError::Mdns(e.to_string()))?;
        debug!(port = announcement.tcp_port, "mDNS service registered");
        self.registered = Some(fullname);
        Ok(())
    }

    fn unregister(&mut self) {
        if let Some(fullname) = self.registered.take() {
            match self.daemon.unregister(&fullname) {
                Ok(_) => debug!("mDNS service unregistered"),
                Err(error) => debug!(%error, "could not unregister the mDNS service"),
            }
        }
    }

    /// Unregisters, stops browsing, and waits briefly for the daemon thread
    /// to exit. The daemon runs commands in order, so the goodbye for our
    /// service goes out before it stops.
    fn shut_down(mut self) {
        self.unregister();
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        match self.daemon.shutdown() {
            Ok(status) => {
                if status.recv_timeout(MDNS_SHUTDOWN_WAIT).is_err() {
                    warn!("the mDNS daemon did not confirm shutdown in time");
                }
            }
            Err(error) => debug!(%error, "mDNS daemon already stopped"),
        }
    }
}

/// Whether `ip` is on a local network rather than the internet. KDE Connect
/// ignores datagrams and connections from any other address, so the link
/// layer applies the same rule to TCP.
///
/// Private means loopback, RFC 1918, IPv4 link-local, CGNAT (100.64.0.0/10),
/// IPv6 link-local (fe80::/10), the deprecated IPv6 site-local (fec0::/10,
/// which Android still counts), or IPv6 unique-local (fc00::/7). IPv4-mapped
/// IPv6 addresses are judged as the IPv4 address they carry.
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (b & 0xc0) == 0x40)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
                || (first & 0xfe00) == 0xfc00
        }
    }
}

fn bind_listener(port: u16) -> io::Result<(UdpSocket, u16)> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Both reuse options, so another KDE Connect implementation on this Mac
    // and we can share the port. On macOS every socket on the port must set
    // SO_REUSEPORT; broadcasts then reach all of them.
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let socket = UdpSocket::from_std(socket.into())?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

async fn listen(socket: UdpSocket, events: mpsc::Sender<DiscoveryEvent>) {
    let mut buffer = vec![0u8; RECV_BUFFER];
    loop {
        let received = tokio::select! {
            () = events.closed() => break,
            received = socket.recv_from(&mut buffer) => received,
        };
        match received {
            Ok((len, from)) => {
                let from = SocketAddr::new(from.ip().to_canonical(), from.port());
                if !is_private_address(from.ip()) {
                    debug!(%from, "dropping a datagram from a non-private address");
                    continue;
                }
                trace!(%from, len, "datagram received");
                let event = DiscoveryEvent::Datagram {
                    from,
                    data: buffer[..len].to_vec(),
                };
                if events.send(event).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                // Errors here are transient (an ICMP error surfacing, an
                // interface going away); pause so a persistent one cannot spin.
                debug!(%error, "UDP receive failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    debug!("UDP listener stopped");
}

async fn browse(
    receiver: mdns_sd::Receiver<ServiceEvent>,
    events: mpsc::Sender<DiscoveryEvent>,
    own_id: Arc<RwLock<Option<String>>>,
) {
    loop {
        let event = tokio::select! {
            () = events.closed() => break,
            event = receiver.recv_async() => event,
        };
        let Ok(event) = event else { break };
        let ServiceEvent::ServiceResolved(service) = event else {
            trace!(?event, "mDNS event");
            continue;
        };
        let own_id = own_id
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(peer) = mdns_peer(&service, own_id.as_deref()) else {
            continue;
        };
        debug!(
            addresses = ?peer.addresses,
            port = peer.port,
            "mDNS peer resolved"
        );
        if events.send(DiscoveryEvent::MdnsPeer(peer)).await.is_err() {
            break;
        }
    }
    debug!("mDNS browsing stopped");
}

/// Turns a resolved service into an event, or `None` if it is our own or has
/// no private address.
fn mdns_peer(service: &mdns_sd::ResolvedService, own_id: Option<&str>) -> Option<MdnsPeer> {
    let instance = instance_name(&service.fullname).to_owned();
    let txt = |key| {
        service
            .get_property_val_str(key)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let txt_id = txt("id");
    let is_own = |id: &str| own_id.is_some_and(|own| own.eq_ignore_ascii_case(id));
    if is_own(&instance) || txt_id.as_deref().is_some_and(is_own) {
        trace!("mDNS resolved our own service");
        return None;
    }
    let mut addresses: Vec<IpAddr> = service
        .addresses
        .iter()
        .map(|scoped| scoped.to_ip_addr().to_canonical())
        .filter(|ip| is_private_address(*ip) && !is_ipv6_link_local(*ip))
        .collect();
    addresses.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addresses.dedup();
    if addresses.is_empty() {
        debug!(port = service.port, "mDNS peer has no private address");
        return None;
    }
    let device_id =
        txt_id.or_else(|| crate::device_id::is_valid(&instance).then(|| instance.clone()));
    Some(MdnsPeer {
        device_id,
        instance,
        device_name: txt("name"),
        device_type: txt("type"),
        protocol_version: txt("protocol").and_then(|v| v.parse().ok()),
        addresses,
        port: service.port,
    })
}

/// `abc._kdeconnect._udp.local.` -> `abc`.
fn instance_name(fullname: &str) -> &str {
    let suffix_len = SERVICE_TYPE.len() + 1;
    if fullname.len() > suffix_len
        && fullname.is_char_boundary(fullname.len() - suffix_len)
        && fullname[fullname.len() - suffix_len..]
            .strip_prefix('.')
            .is_some_and(|rest| rest.eq_ignore_ascii_case(SERVICE_TYPE))
    {
        &fullname[..fullname.len() - suffix_len]
    } else {
        fullname
    }
}

fn is_ipv6_link_local(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

fn service_info(announcement: &Announcement) -> Result<ServiceInfo, mdns_sd::Error> {
    // Our own host name, rather than the Mac's, so we never contend with
    // mDNSResponder for the records of the Mac's name.
    let host: String = announcement
        .device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let host = format!("landline-{host}.local.");
    let protocol = announcement.protocol_version.to_string();
    let name = truncate_utf8(&announcement.device_name, TXT_STRING_MAX - "name=".len());
    let properties = [
        ("id", announcement.device_id.as_str()),
        ("name", name),
        ("type", announcement.device_type.as_str()),
        ("protocol", protocol.as_str()),
    ];
    Ok(ServiceInfo::new(
        SERVICE_TYPE,
        &announcement.device_id,
        &host,
        (),
        announcement.tcp_port,
        &properties[..],
    )?
    .enable_addr_auto())
}

fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `(interface name, broadcast address)` for every up, non-loopback,
/// broadcast-capable IPv4 interface with a private address. Public addresses
/// are skipped: peers ignore datagrams from them, so broadcasting there would
/// only publish our identity.
fn directed_broadcasts() -> Vec<(String, Ipv4Addr)> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            warn!(%error, "could not list network interfaces");
            return Vec::new();
        }
    };
    interfaces
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback() && !i.is_p2p())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if is_private_address(IpAddr::V4(v4.ip)) => {
                let broadcast = v4.broadcast.or_else(|| {
                    (v4.prefixlen < 31)
                        .then(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)))
                })?;
                Some((i.name, broadcast))
            }
            _ => None,
        })
        .collect()
}

/// Sends from fresh ephemeral sockets (one per address family), as the
/// Android app does. They need no state across network changes.
async fn send_all(datagram: &[u8], targets: Vec<(SendTarget, SocketAddr)>) -> SendReport {
    let mut v4: Option<Result<UdpSocket, IoFailure>> = None;
    let mut v6: Option<Result<UdpSocket, IoFailure>> = None;
    let mut attempts = Vec::with_capacity(targets.len());
    for (target, to) in targets {
        let slot = if to.is_ipv4() { &mut v4 } else { &mut v6 };
        let socket = slot.get_or_insert_with(|| {
            send_socket(to.is_ipv4()).map_err(|error| IoFailure::from(&error))
        });
        let result = match socket {
            Ok(socket) => socket
                .send_to(datagram, to)
                .await
                .map(drop)
                .map_err(|error| IoFailure::from(&error)),
            Err(failure) => Err(failure.clone()),
        };
        attempts.push(SendAttempt { target, to, result });
    }
    SendReport { attempts }
}

fn send_socket(ipv4: bool) -> io::Result<UdpSocket> {
    let (domain, local) = if ipv4 {
        (
            Domain::IPV4,
            SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        )
    } else {
        (
            Domain::IPV6,
            SocketAddr::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
        )
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if ipv4 {
        socket.set_broadcast(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&local.into())?;
    UdpSocket::from_std(socket.into())
}

fn log_report(what: &str, report: &SendReport) {
    for (attempt, failure) in report.failures() {
        debug!(to = %attempt.to, error = %failure, "{what} send failed");
    }
    if !report.sent_any() {
        warn!(
            local_network_denied = report.likely_local_network_denied(),
            "every {what} send failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn udp_only(listen_port: u16, send_port: u16) -> DiscoveryConfig {
        DiscoveryConfig {
            listen_port,
            send_port,
            mdns: false,
            mdns_port: DEFAULT_MDNS_PORT,
        }
    }

    fn listening_port(discovery: &Discovery) -> u16 {
        match discovery.listener_state() {
            ListenerState::Listening { port } => port,
            other => panic!("expected a listener, got {other:?}"),
        }
    }

    #[test]
    fn private_addresses() {
        let cases: &[(&str, bool)] = &[
            ("127.0.0.1", true),
            ("127.255.0.9", true),
            ("10.0.0.1", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.32.0.1", false),
            ("192.168.1.20", true),
            ("169.254.10.10", true),
            ("100.64.0.1", true),
            ("100.127.255.255", true),
            ("100.128.0.1", false),
            ("100.63.255.255", false),
            ("8.8.8.8", false),
            ("0.0.0.0", false),
            ("255.255.255.255", false),
            ("::1", true),
            ("fe80::1", true),
            ("febf::1", true),
            ("fec0::1", true),
            ("fc00::1", true),
            ("fd12:3456::1", true),
            ("fe00::1", false),
            ("2001:db8::1", false),
            ("2a00:1450::1", false),
            ("::", false),
            ("::ffff:192.168.1.5", true),
            ("::ffff:10.1.2.3", true),
            ("::ffff:127.0.0.1", true),
            ("::ffff:8.8.8.8", false),
            ("::ffff:100.64.1.1", true),
        ];
        for (text, expected) in cases {
            let ip: IpAddr = text.parse().unwrap();
            assert_eq!(is_private_address(ip), *expected, "{text}");
        }
    }

    #[test]
    fn instance_names_lose_the_service_suffix() {
        assert_eq!(instance_name("abc_123._kdeconnect._udp.local."), "abc_123");
        assert_eq!(instance_name("abc._KDECONNECT._udp.local."), "abc");
        assert_eq!(instance_name("odd"), "odd");
    }

    #[test]
    fn utf8_truncation_keeps_whole_characters() {
        assert_eq!(truncate_utf8("ab", 5), "ab");
        assert_eq!(truncate_utf8("aé", 2), "a");
        assert_eq!(truncate_utf8("aé", 3), "aé");
    }

    fn attempt(target: SendTarget, result: Result<(), io::ErrorKind>) -> SendAttempt {
        SendAttempt {
            target,
            to: "192.168.1.255:1716".parse().unwrap(),
            result: result.map_err(|kind| IoFailure::from(&io::Error::from(kind))),
        }
    }

    #[test]
    fn report_classifies_local_network_denial() {
        use io::ErrorKind::{HostUnreachable, NetworkUnreachable};
        let lan = || SendTarget::DirectedBroadcast {
            interface: "en0".into(),
        };
        let denied = SendReport {
            attempts: vec![
                attempt(SendTarget::LimitedBroadcast, Err(HostUnreachable)),
                attempt(lan(), Err(HostUnreachable)),
            ],
        };
        assert!(!denied.sent_any());
        assert!(denied.likely_local_network_denied());

        let partly = SendReport {
            attempts: vec![
                attempt(SendTarget::LimitedBroadcast, Ok(())),
                attempt(lan(), Err(HostUnreachable)),
            ],
        };
        assert!(partly.sent_any());
        assert!(!partly.likely_local_network_denied());

        let offline = SendReport {
            attempts: vec![attempt(SendTarget::LimitedBroadcast, Err(HostUnreachable))],
        };
        assert!(!offline.likely_local_network_denied());

        let no_network = SendReport {
            attempts: vec![attempt(SendTarget::Unicast, Err(NetworkUnreachable))],
        };
        assert!(!no_network.likely_local_network_denied());
        let unicast_denied = SendReport {
            attempts: vec![attempt(SendTarget::Unicast, Err(HostUnreachable))],
        };
        assert!(unicast_denied.likely_local_network_denied());
    }

    #[test]
    fn start_outside_a_runtime_fails() {
        let (tx, _rx) = mpsc::channel(1);
        assert!(matches!(
            Discovery::start(udp_only(0, 0), tx),
            Err(DiscoveryError::NoRuntime)
        ));
    }

    #[tokio::test]
    async fn loopback_datagram_arrives_as_an_event() {
        let (rx_tx, mut rx) = mpsc::channel(8);
        let receiver = Discovery::start(udp_only(0, 0), rx_tx).unwrap();
        let port = listening_port(&receiver);

        let (tx_tx, _tx_rx) = mpsc::channel(8);
        let sender = Discovery::start(udp_only(0, port), tx_tx).unwrap();
        let datagram = b"{\"type\":\"kdeconnect.identity\"}\n";
        let report = sender
            .send_to(datagram, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert!(report.sent_any(), "{report:?}");
        assert_eq!(report.attempts.len(), 1);
        assert_eq!(report.attempts[0].target, SendTarget::Unicast);

        let event = tokio::time::timeout(WAIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        let DiscoveryEvent::Datagram { from, data } = event else {
            panic!("expected a datagram, got {event:?}");
        };
        assert_eq!(data, datagram);
        assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn ipv4_mapped_unicast_is_sent_as_ipv4() {
        let (rx_tx, mut rx) = mpsc::channel(8);
        let receiver = Discovery::start(udp_only(0, 0), rx_tx).unwrap();
        let (tx_tx, _tx_rx) = mpsc::channel(8);
        let sender = Discovery::start(udp_only(0, listening_port(&receiver)), tx_tx).unwrap();
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let report = sender.send_to(b"x", mapped).await;
        assert!(report.attempts[0].to.is_ipv4());
        assert!(report.sent_any(), "{report:?}");
        let event = tokio::time::timeout(WAIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, DiscoveryEvent::Datagram { data, .. } if data == b"x"));
    }

    #[tokio::test]
    async fn broadcast_reports_every_target() {
        let (tx, _rx) = mpsc::channel(8);
        // Broadcast to a port this socket holds on every address, so no other
        // test's listener can be given it.
        let blocker = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = blocker.local_addr().unwrap().port();
        let discovery = Discovery::start(udp_only(0, port), tx).unwrap();
        let report = discovery.broadcast(b"probe").await;
        assert_eq!(report.attempts[0].target, SendTarget::LimitedBroadcast);
        assert_eq!(
            report.attempts[0].to,
            SocketAddr::from((Ipv4Addr::BROADCAST, port))
        );
        for attempt in &report.attempts[1..] {
            assert!(matches!(
                attempt.target,
                SendTarget::DirectedBroadcast { .. }
            ));
            assert_eq!(attempt.to.port(), port);
        }
    }

    #[tokio::test]
    async fn unbindable_port_means_send_only_until_refresh() {
        // A socket without SO_REUSEPORT holds the port exclusively.
        let blocker = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = blocker.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::channel(8);
        let discovery = Discovery::start(udp_only(port, port), tx).unwrap();
        let ListenerState::SendOnly { error } = discovery.listener_state() else {
            panic!("expected send-only, got {:?}", discovery.listener_state());
        };
        assert_eq!(error.kind, io::ErrorKind::AddrInUse);

        // Sending still works in send-only mode.
        let report = discovery
            .send_to(b"x", IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert!(report.sent_any(), "{report:?}");

        drop(blocker);
        assert_eq!(discovery.refresh(), ListenerState::Listening { port });
        let report = discovery
            .send_to(b"again", IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert!(report.sent_any(), "{report:?}");
        // macOS delivers loopback datagrams asynchronously, so the first
        // send can still land on the new listener; skip it.
        tokio::time::timeout(WAIT, async {
            loop {
                match rx.recv().await {
                    Some(DiscoveryEvent::Datagram { data, .. }) if data == b"again" => break,
                    Some(DiscoveryEvent::Datagram { data, .. }) if data == b"x" => continue,
                    other => panic!("unexpected {other:?}"),
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_discovery_stops_the_listener() {
        let (tx, mut rx) = mpsc::channel(8);
        let discovery = Discovery::start(udp_only(0, 0), tx).unwrap();
        let port = listening_port(&discovery);
        drop(discovery);

        // Every sender clone lives in a task or in `Discovery`, so the
        // channel closes only once all of them are gone.
        let closed = tokio::time::timeout(WAIT, rx.recv()).await.unwrap();
        assert_eq!(closed, None);

        // And nothing holds the port any more: an exclusive bind succeeds.
        std::net::UdpSocket::bind(("0.0.0.0", port)).unwrap();
    }

    #[tokio::test]
    async fn announce_needs_mdns_enabled() {
        let (tx, _rx) = mpsc::channel(8);
        let discovery = Discovery::start(udp_only(0, 0), tx).unwrap();
        assert!(!discovery.mdns_running());
        let result = discovery.announce(Announcement {
            device_id: crate::device_id::generate(),
            device_name: "Test".into(),
            device_type: "laptop".into(),
            protocol_version: 8,
            tcp_port: 1716,
        });
        assert!(matches!(result, Err(DiscoveryError::MdnsDisabled)));
    }

    /// Two instances on a private mDNS port: one announces, the other must
    /// resolve it, and the announcer must not report itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mdns_announce_and_browse() {
        let mdns_port = {
            let probe = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let config = DiscoveryConfig {
            listen_port: 0,
            send_port: 0,
            mdns: true,
            mdns_port,
        };
        let (a_tx, mut a_rx) = mpsc::channel(32);
        let (b_tx, mut b_rx) = mpsc::channel(32);
        let a = Discovery::start(config.clone(), a_tx).unwrap();
        let b = Discovery::start(config, b_tx).unwrap();
        assert!(a.mdns_running() && b.mdns_running());

        let a_id = crate::device_id::generate();
        b.set_own_device_id(crate::device_id::generate());
        a.announce(Announcement {
            device_id: a_id.clone(),
            device_name: "Landline test".into(),
            device_type: "laptop".into(),
            protocol_version: 8,
            tcp_port: 1739,
        })
        .unwrap();

        let peer = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match b_rx.recv().await {
                    Some(DiscoveryEvent::MdnsPeer(peer))
                        if peer.device_id.as_deref() == Some(a_id.as_str()) =>
                    {
                        break peer;
                    }
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("the browser should resolve the announced service");
        assert_eq!(peer.instance, a_id);
        assert_eq!(peer.port, 1739);
        assert_eq!(peer.device_name.as_deref(), Some("Landline test"));
        assert_eq!(peer.device_type.as_deref(), Some("laptop"));
        assert_eq!(peer.protocol_version, Some(8));
        assert!(!peer.addresses.is_empty());
        assert!(peer.addresses.iter().all(|ip| is_private_address(*ip)));
        let first_v6 = peer.addresses.iter().position(IpAddr::is_ipv6);
        let last_v4 = peer.addresses.iter().rposition(IpAddr::is_ipv4);
        if let (Some(v6), Some(v4)) = (first_v6, last_v4) {
            assert!(v4 < v6, "IPv4 first: {:?}", peer.addresses);
        }

        while let Ok(event) = a_rx.try_recv() {
            if let DiscoveryEvent::MdnsPeer(peer) = event {
                assert_ne!(peer.device_id.as_deref(), Some(a_id.as_str()));
            }
        }
        // Dropping in a blocking context: shutdown waits on the daemon thread.
        tokio::task::spawn_blocking(move || drop((a, b)))
            .await
            .unwrap();
    }
}
