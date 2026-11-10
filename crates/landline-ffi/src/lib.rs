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
    /// The device is not connected, or not paired.
    #[error("the device is not connected")]
    NotReachable,
    /// Browsing or transferring the device's files failed.
    #[error("{message}")]
    Storage { message: String },
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
    /// The device's battery, once it has reported it on this connection.
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Battery {
    /// Percent, 0 to 100.
    pub charge: u8,
    pub charging: bool,
}

/// A notification showing on the phone.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PhoneNotification {
    /// Identifies the notification on its device. A later notification with
    /// the same ID replaces this one.
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    /// When the phone posted it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Whether `dismiss_notification` can remove it.
    pub dismissable: bool,
    /// It was already showing on the phone when we connected: list it, but
    /// do not announce it.
    pub silent: bool,
    /// Path of the app's icon as a PNG file, if the phone sent one.
    pub icon_path: Option<String>,
}

/// A text message the phone received.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SmsMessage {
    /// The sender's number or alphanumeric sender ID.
    pub sender: String,
    pub body: String,
    /// When the phone received it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
}

/// A one-time code found in an SMS or a notification.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OtpCode {
    /// The digits only.
    pub code: String,
    /// The app or sender the text came from.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferDirection {
    /// From the device to this Mac.
    Incoming,
    /// From this Mac to the device.
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TransferState {
    InProgress,
    /// For an incoming file, `path` is where it was saved; for an outgoing
    /// one, the file that was sent.
    Finished {
        path: String,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

/// One file moving between the devices.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Transfer {
    /// Unique while the app runs.
    pub id: u64,
    pub device_id: String,
    pub file_name: String,
    pub direction: TransferDirection,
    pub bytes_done: u64,
    /// `None` when the sender did not say how large the file is.
    pub bytes_total: Option<u64>,
    pub state: TransferState,
}

/// A file or folder on the device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RemoteEntry {
    pub name: String,
    /// Absolute path on the device, for further calls.
    pub path: String,
    pub is_dir: bool,
    /// In bytes. Zero for folders.
    pub size: u64,
    /// Seconds since the Unix epoch, if the device reported it.
    pub modified_secs: Option<i64>,
}

/// A place on the device to start browsing from, such as internal storage
/// or an SD card.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StorageRoot {
    pub name: String,
    pub path: String,
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
    /// Where files sent by a device are saved. `None` means the user's
    /// Downloads folder.
    pub download_dir: Option<String>,
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
    /// A notification appeared on the device or changed.
    fn notification_posted(&self, device_id: String, notification: PhoneNotification);
    /// A notification went away on the device.
    fn notification_removed(&self, device_id: String, notification_id: String);
    fn sms_received(&self, device_id: String, message: SmsMessage);
    /// A one-time code arrived in an SMS or a notification. Reported once
    /// per code, even when both carry it.
    fn otp_detected(&self, device_id: String, otp: OtpCode);
    /// The device offered its clipboard text.
    fn clipboard_received(&self, device_id: String, text: String);
    /// A file transfer started, progressed, or ended.
    fn transfer_updated(&self, transfer: Transfer);
    /// The device shared a piece of text.
    fn text_shared(&self, device_id: String, text: String);
    /// The device shared a link.
    fn url_shared(&self, device_id: String, url: String);
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

    fn notification_posted(&self, device_id: &str, n: &landline_core::PhoneNotification) {
        let notification = PhoneNotification {
            id: n.id.clone(),
            app_name: n.app_name.clone(),
            title: n.title.clone(),
            text: n.text.clone(),
            time_ms: n.time_ms,
            dismissable: n.dismissable,
            silent: n.silent,
            icon_path: n.icon_path.as_deref().map(path_string),
        };
        self.0
            .notification_posted(device_id.to_owned(), notification);
    }

    fn notification_removed(&self, device_id: &str, notification_id: &str) {
        self.0
            .notification_removed(device_id.to_owned(), notification_id.to_owned());
    }

    fn sms_received(&self, device_id: &str, message: &landline_core::SmsMessage) {
        let message = SmsMessage {
            sender: message.sender.clone(),
            body: message.body.clone(),
            time_ms: message.time_ms,
        };
        self.0.sms_received(device_id.to_owned(), message);
    }

    fn otp_detected(&self, device_id: &str, otp: &landline_core::OtpCode) {
        let otp = OtpCode {
            code: otp.code.clone(),
            source: otp.source.clone(),
        };
        self.0.otp_detected(device_id.to_owned(), otp);
    }

    fn clipboard_received(&self, device_id: &str, text: &str) {
        self.0
            .clipboard_received(device_id.to_owned(), text.to_owned());
    }

    fn transfer_updated(&self, transfer: &landline_core::Transfer) {
        use landline_core::{TransferDirection as Direction, TransferState as State};
        self.0.transfer_updated(Transfer {
            id: transfer.id,
            device_id: transfer.device_id.clone(),
            file_name: transfer.file_name.clone(),
            direction: match transfer.direction {
                Direction::Incoming => TransferDirection::Incoming,
                Direction::Outgoing => TransferDirection::Outgoing,
            },
            bytes_done: transfer.bytes_done,
            bytes_total: transfer.bytes_total,
            state: match &transfer.state {
                State::InProgress => TransferState::InProgress,
                State::Finished { path } => TransferState::Finished {
                    path: path_string(path),
                },
                State::Failed { reason } => TransferState::Failed {
                    reason: reason.clone(),
                },
                State::Cancelled => TransferState::Cancelled,
            },
        });
    }

    fn text_shared(&self, device_id: &str, text: &str) {
        self.0.text_shared(device_id.to_owned(), text.to_owned());
    }

    fn url_shared(&self, device_id: &str, url: &str) {
        self.0.url_shared(device_id.to_owned(), url.to_owned());
    }
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
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
            battery: device.battery.map(|battery| Battery {
                charge: battery.charge,
                charging: battery.charging,
            }),
        }
    }
}

impl From<landline_core::CoreError> for LandlineError {
    fn from(error: landline_core::CoreError) -> Self {
        match error {
            landline_core::CoreError::UnknownDevice => Self::UnknownDevice,
            landline_core::CoreError::NotReachable => Self::NotReachable,
            landline_core::CoreError::Storage(message) => Self::Storage { message },
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
        let download_dir = config.download_dir;
        let mut config = landline_core::Config::new(config.data_dir, config.device_name);
        if let Some(download_dir) = download_dir {
            config.download_dir = download_dir.into();
        }
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

    /// Removes a notification from the device.
    pub fn dismiss_notification(
        &self,
        device_id: String,
        notification_id: String,
    ) -> Result<(), LandlineError> {
        Ok(self
            .inner
            .dismiss_notification(&device_id, &notification_id)?)
    }

    /// Offers `text` to the clipboard of every paired, connected device.
    pub fn send_clipboard(&self, text: String) {
        self.inner.send_clipboard(&text);
    }

    /// Sends files to the device. Progress arrives through
    /// `transfer_updated`.
    pub fn send_files(&self, device_id: String, paths: Vec<String>) -> Result<(), LandlineError> {
        let paths = paths.into_iter().map(Into::into).collect();
        Ok(self.inner.send_files(&device_id, paths)?)
    }

    /// Stops a transfer with that device. An incoming file's partial copy
    /// is deleted.
    pub fn cancel_transfer(
        &self,
        device_id: String,
        transfer_id: u64,
    ) -> Result<(), LandlineError> {
        Ok(self.inner.cancel_transfer(&device_id, transfer_id)?)
    }

    /// Changes where files sent by a device are saved from now on.
    pub fn set_download_dir(&self, path: String) {
        self.inner.set_download_dir(path);
    }

    /// The places on the device that can be browsed.
    pub async fn storage_roots(
        &self,
        device_id: String,
    ) -> Result<Vec<StorageRoot>, LandlineError> {
        let roots = self.inner.storage_roots(&device_id).await?;
        Ok(roots
            .into_iter()
            .map(|root| StorageRoot {
                name: root.name,
                path: root.path,
            })
            .collect())
    }

    /// Lists a folder on the device.
    pub async fn list_dir(
        &self,
        device_id: String,
        path: String,
    ) -> Result<Vec<RemoteEntry>, LandlineError> {
        let entries = self.inner.list_dir(&device_id, &path).await?;
        Ok(entries
            .into_iter()
            .map(|entry| RemoteEntry {
                name: entry.name,
                path: entry.path,
                is_dir: entry.is_dir,
                size: entry.size,
                modified_secs: entry.modified_secs,
            })
            .collect())
    }

    /// Copies a file from the device to `local`.
    pub async fn download(
        &self,
        device_id: String,
        remote: String,
        local: String,
    ) -> Result<(), LandlineError> {
        Ok(self.inner.download(&device_id, &remote, local).await?)
    }

    /// Copies the file at `local` to `remote` on the device.
    pub async fn upload(
        &self,
        device_id: String,
        local: String,
        remote: String,
    ) -> Result<(), LandlineError> {
        Ok(self.inner.upload(&device_id, local, &remote).await?)
    }
}
