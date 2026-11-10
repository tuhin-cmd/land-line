//! Two cores on the loopback interface, driven through the public API: they
//! find each other, pair, ping, and remember the pairing across a restart.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use landline_core::discovery::DiscoveryConfig;
use landline_core::{Config, Core, CoreDelegate, Device, NetworkStatus, PairState, PairingFailure};

const WAIT: Duration = Duration::from_secs(15);
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Debug)]
enum Event {
    Devices(Vec<Device>),
    PairingFailed(PairingFailure),
    Ping(String),
}

struct Recorder(Sender<Event>);

impl CoreDelegate for Recorder {
    fn devices_changed(&self, devices: Vec<Device>) {
        let _ = self.0.send(Event::Devices(devices));
    }
    fn pairing_failed(&self, _device_id: &str, reason: PairingFailure) {
        let _ = self.0.send(Event::PairingFailed(reason));
    }
    fn ping_received(&self, device_id: &str) {
        let _ = self.0.send(Event::Ping(device_id.to_owned()));
    }
    fn network_status_changed(&self, _status: NetworkStatus) {}
}

struct Peer {
    core: Core,
    events: Receiver<Event>,
}

impl Peer {
    /// A core that listens for identities on `listen_port` and sends its own
    /// to `send_port`, so two of them can be pointed at each other without
    /// touching the real KDE Connect port or the LAN.
    fn start(dir: &Path, name: &str, listen_port: u16, send_port: u16) -> Self {
        let mut config = Config::new(dir, name);
        config.broadcast = false;
        config.discovery = DiscoveryConfig {
            listen_port,
            send_port,
            mdns: false,
            ..DiscoveryConfig::default()
        };
        let (tx, events) = mpsc::channel();
        let core = Core::new(config, Arc::new(Recorder(tx))).unwrap();
        core.start();
        Self { core, events }
    }

    /// Waits for a device list in which `other` satisfies `check`.
    fn wait_for(&self, other: &str, check: impl Fn(&Device) -> bool) -> Device {
        self.wait_for_event(|event| match event {
            Event::Devices(devices) => devices
                .into_iter()
                .find(|device| device.id == other && check(device)),
            _ => None,
        })
    }

    fn wait_for_event<T>(&self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = self
                .events
                .recv_timeout(left)
                .expect("timed out waiting for an event");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    }

    /// Announces to the other core until this one sees it connected. Start
    /// is asynchronous, so the first announcement can go out before the
    /// other side listens.
    fn connect_to(&self, other: &Peer) -> Device {
        let other_id = other.core.device_id().to_owned();
        let deadline = Instant::now() + WAIT;
        loop {
            self.core.announce_to(LOOPBACK);
            while let Ok(event) = self.events.recv_timeout(Duration::from_millis(300)) {
                if let Event::Devices(devices) = event
                    && let Some(device) = devices
                        .into_iter()
                        .find(|device| device.id == other_id && device.reachable)
                {
                    return device;
                }
            }
            assert!(Instant::now() < deadline, "the devices never connected");
        }
    }
}

/// Two UDP ports that are free right now.
fn free_ports() -> (u16, u16) {
    let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let b = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    )
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "landline-test-{name}-{}-{}",
        std::process::id(),
        landline_core::device_id::generate()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn devices_connect_pair_ping_and_remember() {
    let (mac_dir, phone_dir) = (temp_dir("mac"), temp_dir("phone"));
    let (mac_port, phone_port) = free_ports();
    let mac = Peer::start(&mac_dir, "Test Mac", mac_port, phone_port);
    let phone = Peer::start(&phone_dir, "Test Phone", phone_port, mac_port);
    let (mac_id, phone_id) = (
        mac.core.device_id().to_owned(),
        phone.core.device_id().to_owned(),
    );

    // Connecting: each side lists the other, unpaired and reachable.
    let seen = mac.connect_to(&phone);
    assert_eq!(seen.name, "Test Phone");
    assert_eq!(seen.pair_state, PairState::NotPaired);
    assert_eq!(seen.verification_key, None);
    phone.wait_for(&mac_id, |device| device.reachable);

    // A ping from an unpaired device is not delivered.
    mac.core.send_ping(&phone_id).unwrap();

    // Pairing: both sides show the same key, then both end up paired.
    mac.core.request_pair(&phone_id).unwrap();
    let asking = mac.wait_for(&phone_id, |d| d.pair_state == PairState::Requested);
    let asked = phone.wait_for(&mac_id, |d| d.pair_state == PairState::RequestedByPeer);
    let key = asking.verification_key.expect("the requester shows a key");
    assert_eq!(key.len(), 8);
    assert_eq!(asked.verification_key.as_deref(), Some(key.as_str()));

    phone.core.accept_pair(&mac_id).unwrap();
    let paired = mac.wait_for(&phone_id, |d| d.pair_state == PairState::Paired);
    assert_eq!(paired.verification_key, None);
    phone.wait_for(&mac_id, |d| d.pair_state == PairState::Paired);

    // Pings flow both ways once paired, and the earlier one never arrived.
    mac.core.send_ping(&phone_id).unwrap();
    let from = phone.wait_for_event(|event| match event {
        Event::Ping(from) => Some(from),
        _ => None,
    });
    assert_eq!(from, mac_id);
    phone.core.send_ping(&mac_id).unwrap();
    mac.wait_for_event(|event| matches!(event, Event::Ping(_)).then_some(()));

    // Restart the Mac side: the phone is listed as paired before it
    // connects, and reconnects under the pinned certificate.
    drop(mac);
    phone.wait_for(&mac_id, |d| {
        !d.reachable && d.pair_state == PairState::Paired
    });
    let mac = Peer::start(&mac_dir, "Test Mac", mac_port, phone_port);
    assert_eq!(mac.core.device_id(), mac_id);
    let remembered = mac.core.devices();
    assert_eq!(remembered.len(), 1);
    assert_eq!(remembered[0].pair_state, PairState::Paired);
    assert!(!remembered[0].reachable);

    let back = mac.connect_to(&phone);
    assert_eq!(back.pair_state, PairState::Paired);
    phone.wait_for(&mac_id, |d| {
        d.reachable && d.pair_state == PairState::Paired
    });

    // Unpairing reaches the other side, and the devices stay connected.
    phone.core.unpair(&mac_id).unwrap();
    mac.wait_for(&phone_id, |d| {
        d.pair_state == PairState::NotPaired && d.reachable
    });

    drop((mac, phone));
    let _ = std::fs::remove_dir_all(mac_dir);
    let _ = std::fs::remove_dir_all(phone_dir);
}

#[test]
fn a_rejected_request_is_reported_to_the_requester() {
    let (mac_dir, phone_dir) = (temp_dir("mac"), temp_dir("phone"));
    let (mac_port, phone_port) = free_ports();
    let mac = Peer::start(&mac_dir, "Test Mac", mac_port, phone_port);
    let phone = Peer::start(&phone_dir, "Test Phone", phone_port, mac_port);
    let (mac_id, phone_id) = (
        mac.core.device_id().to_owned(),
        phone.core.device_id().to_owned(),
    );

    mac.connect_to(&phone);
    phone.wait_for(&mac_id, |device| device.reachable);
    mac.core.request_pair(&phone_id).unwrap();
    phone.wait_for(&mac_id, |d| d.pair_state == PairState::RequestedByPeer);
    phone.core.reject_pair(&mac_id).unwrap();

    let reason = mac.wait_for_event(|event| match event {
        Event::PairingFailed(reason) => Some(reason),
        _ => None,
    });
    assert_eq!(reason, PairingFailure::CancelledByPeer);
    assert_eq!(mac.core.devices()[0].pair_state, PairState::NotPaired);
    assert!(matches!(
        mac.core.request_pair("0123456789abcdef0123456789abcdef"),
        Err(landline_core::CoreError::UnknownDevice)
    ));

    drop((mac, phone));
    let _ = std::fs::remove_dir_all(mac_dir);
    let _ = std::fs::remove_dir_all(phone_dir);
}
