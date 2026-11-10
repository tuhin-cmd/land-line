//! Landline core: the KDE Connect protocol, transport, and plugins.
//!
//! The core owns the protocol and all network state, and never calls a macOS
//! API. It runs its own tokio runtime, so callers need no async runtime of
//! their own: every method returns immediately, and results arrive through
//! the [`CoreDelegate`] on core threads.

#![forbid(unsafe_code)]

pub mod device_id;
pub mod discovery;
pub mod identity;
mod link;
mod manager;
pub mod packet;
pub mod pairing;
pub mod store;
pub mod tls;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::discovery::DiscoveryConfig;
use crate::manager::{Command, Manager, Msg};
use crate::store::{LocalIdentity, TrustStore};

pub use crate::identity::DeviceType;
pub use crate::packet::Packet;
pub use crate::pairing::{PairState, PairingFailure};

/// Version of the core, as set in the workspace manifest.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("could not start the core runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("could not use the data directory: {0}")]
    Store(#[from] store::StoreError),
    /// No device with this ID is known. It may have gone away since the last
    /// [`CoreDelegate::devices_changed`].
    #[error("unknown device")]
    UnknownDevice,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Holds this device's certificate and the paired-device list. Created
    /// if missing.
    pub data_dir: PathBuf,
    /// Name shown on other devices.
    pub device_name: String,
    pub device_type: DeviceType,
    pub discovery: DiscoveryConfig,
    /// Whether to announce by UDP broadcast. Tests turn this off to stay off
    /// the LAN and reach each other with [`Core::announce_to`] instead.
    pub broadcast: bool,
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>, device_name: impl Into<String>) -> Self {
        Self {
            data_dir: data_dir.into(),
            device_name: device_name.into(),
            device_type: DeviceType::Laptop,
            discovery: DiscoveryConfig::default(),
            broadcast: true,
        }
    }
}

/// A device the core knows about: every paired device, reachable or not, and
/// every unpaired device currently connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub device_type: DeviceType,
    /// Whether there is a live connection to the device right now.
    pub reachable: bool,
    pub pair_state: PairState,
    /// Shown on both devices while pairing is pending in either direction,
    /// so the user can check they match. `None` otherwise.
    pub verification_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkStatus {
    /// Our announcement went out on at least one interface.
    Ready,
    /// There is no usable network interface or port.
    Offline,
    /// Every send was refused. On macOS that means the app lacks the Local
    /// Network permission; no device can be found until it is granted.
    Blocked,
}

/// Receives events from the core. Methods are called on core threads and
/// must return quickly; hop to another thread for anything slow.
pub trait CoreDelegate: Send + Sync + 'static {
    /// The full device list, sent whenever anything in it changes.
    fn devices_changed(&self, devices: Vec<Device>);
    /// A pairing attempt ended without pairing. The device is already back
    /// to `NotPaired` in the list.
    fn pairing_failed(&self, device_id: &str, reason: PairingFailure);
    fn ping_received(&self, device_id: &str);
    fn network_status_changed(&self, status: NetworkStatus);
    /// Every packet from a paired device other than pairing itself. Bodies
    /// can hold private content, so do not log them.
    fn packet_received(&self, _device_id: &str, _packet: &Packet) {}
}

pub struct Core {
    // Dropping the runtime stops the manager and closes every socket. It
    // must not happen on a thread that is running async code.
    _runtime: Runtime,
    tx: UnboundedSender<Msg>,
    devices: Arc<Mutex<Vec<Device>>>,
    device_id: String,
}

impl Core {
    /// Loads or creates this device's identity in `config.data_dir`. Nothing
    /// touches the network until [`Core::start`].
    pub fn new(config: Config, delegate: Arc<dyn CoreDelegate>) -> Result<Self, CoreError> {
        let (local, created) = LocalIdentity::load_or_create(&config.data_dir)?;
        let mut trust = TrustStore::load(&config.data_dir)?;
        if created {
            // Peers pinned the old certificate, so every pairing is void.
            trust.clear()?;
        }

        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("landline-core")
            .enable_all()
            .build()
            .map_err(CoreError::Runtime)?;

        let device_id = local.device_id().to_owned();
        let devices = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let manager = Manager::new(
            config,
            local,
            trust,
            delegate,
            Arc::clone(&devices),
            tx.clone(),
        );
        runtime.spawn(manager.run(rx));

        Ok(Self {
            _runtime: runtime,
            tx,
            devices,
            device_id,
        })
    }

    /// This device's ID on the network.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Starts discovery and accepts connections. Does nothing if started.
    pub fn start(&self) {
        self.send(Msg::Start);
    }

    /// Drops every connection and stops discovery. Does nothing if stopped.
    pub fn stop(&self) {
        self.send(Msg::Stop);
    }

    /// Call when the network may have changed (Wi-Fi toggled, wake from
    /// sleep, new address) so the core announces itself again.
    pub fn network_changed(&self) {
        self.send(Msg::NetworkChanged);
    }

    /// Sends our identity straight to `address`, for networks that drop
    /// broadcasts. A KDE Connect device there will connect back.
    pub fn announce_to(&self, address: IpAddr) {
        self.send(Msg::AnnounceTo(address));
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Asks the device to pair, or accepts if it has already asked us.
    pub fn request_pair(&self, device_id: &str) -> Result<(), CoreError> {
        self.command(device_id, Command::RequestPair)
    }

    pub fn accept_pair(&self, device_id: &str) -> Result<(), CoreError> {
        self.command(device_id, Command::AcceptPair)
    }

    /// Rejects a request from the device, or cancels our own.
    pub fn reject_pair(&self, device_id: &str) -> Result<(), CoreError> {
        self.command(device_id, Command::RejectPair)
    }

    pub fn unpair(&self, device_id: &str) -> Result<(), CoreError> {
        self.command(device_id, Command::Unpair)
    }

    /// Pings a paired, reachable device. Does nothing otherwise.
    pub fn send_ping(&self, device_id: &str) -> Result<(), CoreError> {
        self.command(device_id, Command::Ping)
    }

    fn command(&self, device_id: &str, command: Command) -> Result<(), CoreError> {
        let known = self
            .devices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|device| device.id == device_id);
        if !known {
            return Err(CoreError::UnknownDevice);
        }
        self.send(Msg::Command {
            device_id: device_id.to_owned(),
            command,
        });
        Ok(())
    }

    fn send(&self, msg: Msg) {
        // The manager lives as long as the runtime this struct owns.
        let _ = self.tx.send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_manifest() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
