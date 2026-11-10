//! UniFFI surface for `landline-core`.
//!
//! This crate adapts core types for UniFFI and holds no logic. Anything that
//! decides something belongs in the core.

use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Version of the Rust core.
#[uniffi::export]
pub fn core_version() -> String {
    landline_core::version().to_owned()
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum LandlineError {
    /// The core could not start: its data directory is unusable, or its
    /// identity could not be loaded or created.
    #[error("could not start: {message}")]
    Startup { message: String },
    /// No device with this ID is known. It may have gone away since the last
    /// `devices_changed`.
    #[error("unknown device")]
    UnknownDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeviceKind {
    Phone,
    Tablet,
    Desktop,
    Laptop,
    Tv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairState {
    NotPaired,
    /// We asked to pair and are waiting for the other device to accept.
    Requested,
    /// The other device asked to pair and is waiting for us.
    RequestedByPeer,
    Paired,
}

/// A device the core knows about: every paired device, reachable or not, and
/// every unpaired device currently visible on the network.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    /// Whether there is a live connection to the device right now.
    pub reachable: bool,
    pub pair_state: PairState,
    /// Eight characters shown on both devices while pairing is pending in
    /// either direction, so the user can check they match. `None` otherwise.
    pub verification_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairFailure {
    TimedOut,
    /// The other device rejected the request or cancelled its own.
    CancelledByPeer,
    /// The two devices' clocks differ by more than the protocol allows.
    ClocksDoNotMatch,
    /// The connection dropped before pairing finished.
    NotReachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NetworkStatus {
    /// Discovery is running and can send on at least one interface.
    Ready,
    /// There is no usable network interface.
    Offline,
    /// Every send is being refused, which on macOS means the app lacks the
    /// Local Network permission. Devices cannot be found until it is granted.
    Blocked,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// Directory for this device's certificate and the paired-device list.
    /// Created if missing.
    pub data_dir: String,
    /// Name shown on the other device.
    pub device_name: String,
}

/// Implemented in Swift. Methods are called on core threads.
#[uniffi::export(callback_interface)]
pub trait CoreDelegate: Send + Sync {
    /// The full device list, sent whenever anything in it changes.
    fn devices_changed(&self, devices: Vec<Device>);
    /// A pairing attempt ended without pairing. The device's `pair_state` has
    /// already gone back to `NotPaired` in a preceding `devices_changed`.
    fn pairing_failed(&self, device_id: String, reason: PairFailure);
    fn ping_received(&self, device_id: String);
    fn network_status_changed(&self, status: NetworkStatus);
}

struct DelegateAdapter(Box<dyn CoreDelegate>);

impl landline_core::CoreDelegate for DelegateAdapter {
    fn devices_changed(&self, devices: Vec<landline_core::Device>) {
        self.0
            .devices_changed(devices.into_iter().map(Device::from).collect());
    }

    fn pairing_failed(&self, device_id: &str, reason: landline_core::PairingFailure) {
        use landline_core::PairingFailure as Core;
        let reason = match reason {
            Core::TimedOut => PairFailure::TimedOut,
            Core::CancelledByPeer => PairFailure::CancelledByPeer,
            Core::ClocksDoNotMatch => PairFailure::ClocksDoNotMatch,
            Core::NotReachable => PairFailure::NotReachable,
            // The core does not report the user's own cancellation.
            Core::CancelledByUser => return,
        };
        self.0.pairing_failed(device_id.to_owned(), reason);
    }

    fn ping_received(&self, device_id: &str) {
        self.0.ping_received(device_id.to_owned());
    }

    fn network_status_changed(&self, status: landline_core::NetworkStatus) {
        use landline_core::NetworkStatus as Core;
        self.0.network_status_changed(match status {
            Core::Ready => NetworkStatus::Ready,
            Core::Offline => NetworkStatus::Offline,
            Core::Blocked => NetworkStatus::Blocked,
        });
    }
}

impl From<landline_core::Device> for Device {
    fn from(device: landline_core::Device) -> Self {
        use landline_core::{DeviceType, PairState as Core};
        Self {
            id: device.id,
            name: device.name,
            kind: match device.device_type {
                DeviceType::Phone => DeviceKind::Phone,
                DeviceType::Tablet => DeviceKind::Tablet,
                DeviceType::Desktop => DeviceKind::Desktop,
                DeviceType::Laptop => DeviceKind::Laptop,
                DeviceType::Tv => DeviceKind::Tv,
            },
            reachable: device.reachable,
            pair_state: match device.pair_state {
                Core::NotPaired => PairState::NotPaired,
                Core::Requested => PairState::Requested,
                Core::RequestedByPeer => PairState::RequestedByPeer,
                Core::Paired => PairState::Paired,
            },
            verification_key: device.verification_key,
        }
    }
}

impl From<landline_core::CoreError> for LandlineError {
    fn from(error: landline_core::CoreError) -> Self {
        match error {
            landline_core::CoreError::UnknownDevice => Self::UnknownDevice,
            other => Self::Startup {
                message: other.to_string(),
            },
        }
    }
}

#[derive(uniffi::Object)]
pub struct LandlineCore {
    inner: landline_core::Core,
}

#[uniffi::export]
impl LandlineCore {
    /// Loads or creates this device's identity. Nothing touches the network
    /// until `start`.
    #[uniffi::constructor]
    pub fn new(
        config: CoreConfig,
        delegate: Box<dyn CoreDelegate>,
    ) -> Result<Arc<Self>, LandlineError> {
        let config = landline_core::Config::new(config.data_dir, config.device_name);
        let inner = landline_core::Core::new(config, Arc::new(DelegateAdapter(delegate)))?;
        Ok(Arc::new(Self { inner }))
    }

    /// Starts discovery and accepts connections. Does nothing if started.
    pub fn start(&self) {
        self.inner.start();
    }

    /// Drops every connection and stops discovery. Does nothing if stopped.
    pub fn stop(&self) {
        self.inner.stop();
    }

    /// Call when the network may have changed (Wi-Fi toggled, wake from
    /// sleep, new address) so the core announces itself again.
    pub fn network_changed(&self) {
        self.inner.network_changed();
    }

    pub fn devices(&self) -> Vec<Device> {
        self.inner.devices().into_iter().map(Device::from).collect()
    }

    /// Asks the device to pair, or accepts if it has already asked us.
    pub fn request_pair(&self, device_id: String) -> Result<(), LandlineError> {
        Ok(self.inner.request_pair(&device_id)?)
    }

    pub fn accept_pair(&self, device_id: String) -> Result<(), LandlineError> {
        Ok(self.inner.accept_pair(&device_id)?)
    }

    /// Rejects a request from the device, or cancels our own.
    pub fn reject_pair(&self, device_id: String) -> Result<(), LandlineError> {
        Ok(self.inner.reject_pair(&device_id)?)
    }

    pub fn unpair(&self, device_id: String) -> Result<(), LandlineError> {
        Ok(self.inner.unpair(&device_id)?)
    }

    /// Pings a paired, reachable device. Does nothing otherwise.
    pub fn send_ping(&self, device_id: String) -> Result<(), LandlineError> {
        Ok(self.inner.send_ping(&device_id)?)
    }
}
