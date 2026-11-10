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
pub mod otp;
pub mod packet;
pub mod pairing;
mod payload;
mod plugins;
pub mod store;
pub mod tls;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

use crate::discovery::DiscoveryConfig;
use crate::manager::{Command, Manager, Msg};
use crate::store::{LocalIdentity, TrustStore};

pub use crate::identity::DeviceType;
pub use crate::otp::OtpCode;
pub use crate::packet::Packet;
pub use crate::pairing::{PairState, PairingFailure};
pub use crate::plugins::notifications::PhoneNotification;
pub use crate::plugins::sftp::{RemoteEntry, StorageRoot};
pub use crate::plugins::share::{Transfer, TransferDirection, TransferState};
pub use crate::plugins::sms::SmsMessage;

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
    /// The device is not connected, or not paired.
    #[error("the device is not connected")]
    NotReachable,
    /// Browsing or transferring the device's files failed.
    #[error("{0}")]
    Storage(String),
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
    /// Where files sent by a device are saved. Created when first needed.
    /// [`Core::set_download_dir`] changes it later.
    pub download_dir: PathBuf,
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>, device_name: impl Into<String>) -> Self {
        let data_dir = data_dir.into();
        let download_dir = std::env::home_dir()
            .map_or_else(|| data_dir.join("Received"), |home| home.join("Downloads"));
        Self {
            download_dir,
            data_dir,
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
    /// The device's battery, once it has reported it on this connection.
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Percent, 0 to 100.
    pub charge: u8,
    pub charging: bool,
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
    /// A notification appeared on the device or changed.
    fn notification_posted(&self, _device_id: &str, _notification: &PhoneNotification) {}
    /// A notification went away on the device.
    fn notification_removed(&self, _device_id: &str, _notification_id: &str) {}
    fn sms_received(&self, _device_id: &str, _message: &SmsMessage) {}
    /// A one-time code arrived in an SMS or a notification. Reported once
    /// per code, even when both carry it.
    fn otp_detected(&self, _device_id: &str, _otp: &OtpCode) {}
    /// The device offered its clipboard text.
    fn clipboard_received(&self, _device_id: &str, _text: &str) {}
    /// A file transfer started, progressed, or ended.
    fn transfer_updated(&self, _transfer: &Transfer) {}
    /// The device shared a piece of text.
    fn text_shared(&self, _device_id: &str, _text: &str) {}
    /// The device shared a link.
    fn url_shared(&self, _device_id: &str, _url: &str) {}
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
        self.plugin(device_id, plugins::Command::Ping)
    }

    /// Removes a notification from the device.
    pub fn dismiss_notification(
        &self,
        device_id: &str,
        notification_id: &str,
    ) -> Result<(), CoreError> {
        let id = notification_id.to_owned();
        self.plugin(device_id, plugins::Command::DismissNotification { id })
    }

    /// Offers `text` to the clipboard of every paired, connected device.
    pub fn send_clipboard(&self, text: &str) {
        self.send(Msg::Clipboard(text.to_owned()));
    }

    /// Sends files to the device. Progress arrives through
    /// [`CoreDelegate::transfer_updated`].
    pub fn send_files(&self, device_id: &str, paths: Vec<PathBuf>) -> Result<(), CoreError> {
        self.plugin(device_id, plugins::Command::SendFiles { paths })
    }

    /// Stops a transfer with that device. An incoming file's partial copy
    /// is deleted.
    pub fn cancel_transfer(&self, device_id: &str, transfer_id: u64) -> Result<(), CoreError> {
        let id = transfer_id;
        self.plugin(device_id, plugins::Command::CancelTransfer { id })
    }

    /// Changes where files sent by a device are saved from now on.
    pub fn set_download_dir(&self, path: impl Into<PathBuf>) {
        self.send(Msg::SetDownloadDir(path.into()));
    }

    /// The places on the device that can be browsed.
    pub async fn storage_roots(&self, device_id: &str) -> Result<Vec<StorageRoot>, CoreError> {
        self.storage(device_id, |reply| plugins::sftp::Request::Roots { reply })
            .await
    }

    /// Lists a folder on the device.
    pub async fn list_dir(
        &self,
        device_id: &str,
        path: &str,
    ) -> Result<Vec<RemoteEntry>, CoreError> {
        let path = path.to_owned();
        self.storage(device_id, |reply| plugins::sftp::Request::List {
            path,
            reply,
        })
        .await
    }

    /// Copies a file from the device to `local`.
    pub async fn download(
        &self,
        device_id: &str,
        remote: &str,
        local: impl Into<PathBuf>,
    ) -> Result<(), CoreError> {
        let (remote, local) = (remote.to_owned(), local.into());
        self.storage(device_id, |reply| plugins::sftp::Request::Download {
            remote,
            local,
            reply,
        })
        .await
    }

    /// Copies the file at `local` to `remote` on the device.
    pub async fn upload(
        &self,
        device_id: &str,
        local: impl Into<PathBuf>,
        remote: &str,
    ) -> Result<(), CoreError> {
        let (local, remote) = (local.into(), remote.to_owned());
        self.storage(device_id, |reply| plugins::sftp::Request::Upload {
            local,
            remote,
            reply,
        })
        .await
    }

    /// Sends a storage request and waits for its answer. The work runs on
    /// the core's runtime, so this can be awaited from any executor.
    async fn storage<T>(
        &self,
        device_id: &str,
        request: impl FnOnce(plugins::sftp::Reply<T>) -> plugins::sftp::Request,
    ) -> Result<T, CoreError> {
        let (reply, answer) = oneshot::channel();
        self.plugin(device_id, plugins::Command::Storage(request(reply)))?;
        // A dropped reply means the device went away mid-request.
        answer.await.unwrap_or(Err(CoreError::NotReachable))
    }

    fn plugin(&self, device_id: &str, command: plugins::Command) -> Result<(), CoreError> {
        self.check_known(device_id)?;
        self.send(Msg::Plugin {
            device_id: device_id.to_owned(),
            command,
        });
        Ok(())
    }

    fn check_known(&self, device_id: &str) -> Result<(), CoreError> {
        let known = self
            .devices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|device| device.id == device_id);
        if known {
            Ok(())
        } else {
            Err(CoreError::UnknownDevice)
        }
    }

    fn command(&self, device_id: &str, command: Command) -> Result<(), CoreError> {
        self.check_known(device_id)?;
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
