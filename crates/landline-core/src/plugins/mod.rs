//! Plugins: one module per KDE Connect feature.
//!
//! Each device has its own instance of every plugin. The manager calls a
//! plugin only while the device is usable (paired and connected), from its
//! single task, so plugins need no locking of their own. Work that takes
//! time (payload transfers, SFTP) runs in tasks the plugin spawns, holding
//! clones of what [`Context::link`] and [`Context::delegate`] provide.

// Parts of this are unused until every plugin is implemented; remove this
// once they are.
#![allow(dead_code)]

pub(crate) mod battery;
pub(crate) mod clipboard;
pub(crate) mod notifications;
pub(crate) mod ping;
pub(crate) mod sftp;
pub(crate) mod share;
pub(crate) mod sms;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::packet::Packet;
use crate::payload::{Incoming, Peer};
use crate::{Battery, CoreDelegate, OtpCode};

/// One feature's handling of one device.
pub(crate) trait Plugin: Send {
    /// The device became usable: it is paired and a link is up. Also called
    /// when a link replaces another, after [`Plugin::disconnected`].
    fn connected(&mut self, _ctx: &mut Context<'_>) {}

    /// The device stopped being usable: the link dropped or it was unpaired.
    /// `ctx.link` is already `None`.
    fn disconnected(&mut self, _ctx: &mut Context<'_>) {}

    /// A packet of one of the types this plugin registered as incoming.
    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>);

    /// A command from the app that [`Command::plugin`] routes here. Only
    /// called while the device is usable.
    fn command(&mut self, _command: Command, _ctx: &mut Context<'_>) {}
}

/// What the manager needs to know about a plugin.
pub(crate) struct Registration {
    pub name: &'static str,
    /// Packet types the plugin handles. The peer sends nothing else.
    pub incoming: &'static [&'static str],
    /// Packet types the plugin may send.
    pub outgoing: &'static [&'static str],
    pub create: fn() -> Box<dyn Plugin>,
}

pub(crate) const REGISTRY: &[Registration] = &[
    ping::REGISTRATION,
    battery::REGISTRATION,
    notifications::REGISTRATION,
    sms::REGISTRATION,
    clipboard::REGISTRATION,
    share::REGISTRATION,
    sftp::REGISTRATION,
];

/// Something the app asked one device's plugin to do.
pub(crate) enum Command {
    Ping,
    DismissNotification { id: String },
    SendClipboard { text: String },
    SendFiles { paths: Vec<PathBuf> },
    CancelTransfer { id: u64 },
    Storage(sftp::Request),
}

impl Command {
    /// The plugin that handles this command, by [`Registration::name`].
    pub(crate) fn plugin(&self) -> &'static str {
        match self {
            Self::Ping => ping::REGISTRATION.name,
            Self::DismissNotification { .. } => notifications::REGISTRATION.name,
            Self::SendClipboard { .. } => clipboard::REGISTRATION.name,
            Self::SendFiles { .. } | Self::CancelTransfer { .. } => share::REGISTRATION.name,
            Self::Storage(_) => sftp::REGISTRATION.name,
        }
    }
}

/// Sends packets on a device's link. Cheap to clone into tasks. Sending
/// fails once that link is gone; it does not follow the device to a new one.
#[derive(Clone)]
pub(crate) struct PacketSender(pub(crate) UnboundedSender<Packet>);

impl PacketSender {
    /// Queues `packet` for the device. `false` if the link is gone.
    pub(crate) fn send(&self, packet: Packet) -> bool {
        self.0.send(packet).is_ok()
    }
}

/// The live connection to a device, as plugins and their tasks use it.
#[derive(Clone)]
pub(crate) struct LinkContext {
    pub sender: PacketSender,
    /// For payload connections and anything else that dials the device.
    pub peer: Peer,
}

/// Per-device facts that plugins set and the device list shows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DeviceStatus {
    pub battery: Option<Battery>,
}

/// One-time codes seen recently on a device. The same code usually arrives
/// twice, as an SMS and as the messaging app's notification.
#[derive(Default)]
pub(crate) struct RecentOtps(Vec<(String, Instant)>);

impl RecentOtps {
    const WINDOW: Duration = Duration::from_secs(120);

    /// Records `code`; `true` if it was not seen within the window.
    fn insert(&mut self, code: &str) -> bool {
        let now = Instant::now();
        self.0
            .retain(|(_, seen)| now.duration_since(*seen) < Self::WINDOW);
        if self.0.iter().any(|(seen, _)| seen == code) {
            return false;
        }
        self.0.push((code.to_owned(), now));
        true
    }
}

/// What a plugin can see and do during one call.
pub(crate) struct Context<'a> {
    pub device_id: &'a str,
    pub device_name: &'a str,
    pub delegate: &'a Arc<dyn CoreDelegate>,
    /// Where received files go. The app can change it at any time.
    pub download_dir: &'a Path,
    /// For caches (notification icons). Inside the core's data directory.
    pub cache_dir: &'a Path,
    /// `None` when the device is not connected.
    pub link: Option<LinkContext>,
    /// Set fields here; the manager publishes the device list if it changed.
    pub status: &'a mut DeviceStatus,
    pub(crate) recent_otps: &'a mut RecentOtps,
}

impl Context<'_> {
    /// Queues `packet` for the device. `false` if it is not connected.
    pub(crate) fn send(&self, packet: Packet) -> bool {
        self.link
            .as_ref()
            .is_some_and(|link| link.sender.send(packet))
    }

    /// The payload `packet` offers, if it offers one and the device is
    /// connected. Open it in a spawned task, not in the plugin call.
    pub(crate) fn payload(&self, packet: &Packet) -> Option<Incoming> {
        let link = self.link.as_ref()?;
        Incoming::from_packet(packet, link.peer.clone())
    }

    /// Looks for a one-time code in `text` and reports it to the delegate,
    /// once per code. `source` is the app or sender the text came from.
    pub(crate) fn scan_for_otp(&mut self, source: &str, text: &str) {
        let Some(code) = crate::otp::extract(text) else {
            return;
        };
        if self.recent_otps.insert(&code) {
            let otp = OtpCode {
                code,
                source: source.to_owned(),
            };
            self.delegate.otp_detected(self.device_id, &otp);
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A stand-in for the manager, so a plugin can be tested on its own.

    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::*;
    use crate::store::LocalIdentity;
    use crate::{Device, NetworkStatus, PairingFailure, PhoneNotification, SmsMessage, Transfer};

    /// A delegate call a plugin made.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Event {
        Ping,
        NotificationPosted(PhoneNotification),
        NotificationRemoved(String),
        Sms(SmsMessage),
        Otp(OtpCode),
        Clipboard(String),
        Transfer(Transfer),
        TextShared(String),
        UrlShared(String),
    }

    #[derive(Default)]
    pub(crate) struct Recorder(Mutex<Vec<Event>>);

    impl Recorder {
        fn push(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }

        /// Everything recorded since the last call.
        pub(crate) fn take(&self) -> Vec<Event> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    impl CoreDelegate for Recorder {
        fn devices_changed(&self, _devices: Vec<Device>) {}
        fn pairing_failed(&self, _device_id: &str, _reason: PairingFailure) {}
        fn network_status_changed(&self, _status: NetworkStatus) {}
        fn ping_received(&self, _device_id: &str) {
            self.push(Event::Ping);
        }
        fn notification_posted(&self, _device_id: &str, notification: &PhoneNotification) {
            self.push(Event::NotificationPosted(notification.clone()));
        }
        fn notification_removed(&self, _device_id: &str, notification_id: &str) {
            self.push(Event::NotificationRemoved(notification_id.to_owned()));
        }
        fn sms_received(&self, _device_id: &str, message: &SmsMessage) {
            self.push(Event::Sms(message.clone()));
        }
        fn otp_detected(&self, _device_id: &str, otp: &OtpCode) {
            self.push(Event::Otp(otp.clone()));
        }
        fn clipboard_received(&self, _device_id: &str, text: &str) {
            self.push(Event::Clipboard(text.to_owned()));
        }
        fn transfer_updated(&self, transfer: &Transfer) {
            self.push(Event::Transfer(transfer.clone()));
        }
        fn text_shared(&self, _device_id: &str, text: &str) {
            self.push(Event::TextShared(text.to_owned()));
        }
        fn url_shared(&self, _device_id: &str, url: &str) {
            self.push(Event::UrlShared(url.to_owned()));
        }
    }

    /// Owns everything a [`Context`] borrows. Starts connected, with a peer
    /// on the loopback address whose "pinned" certificate is `peer_identity`.
    pub(crate) struct Harness {
        pub recorder: Arc<Recorder>,
        delegate: Arc<dyn CoreDelegate>,
        pub status: DeviceStatus,
        recent_otps: RecentOtps,
        pub download_dir: PathBuf,
        pub cache_dir: PathBuf,
        link: Option<LinkContext>,
        sent: UnboundedReceiver<Packet>,
        /// This side's identity.
        pub local: Arc<LocalIdentity>,
        /// The identity the fake device would present. Use it to play the
        /// device's side of a payload transfer.
        pub peer_identity: Arc<LocalIdentity>,
    }

    impl Harness {
        pub(crate) const DEVICE_ID: &'static str = "0123456789abcdef0123456789abcdef";
        pub(crate) const DEVICE_NAME: &'static str = "Test Phone";

        pub(crate) fn new() -> Self {
            let recorder = Arc::new(Recorder::default());
            let local = Arc::new(LocalIdentity::generate().unwrap());
            let peer_identity = Arc::new(LocalIdentity::generate().unwrap());
            let (tx, sent) = mpsc::unbounded_channel();
            let scratch = std::env::temp_dir().join(format!(
                "landline-plugin-test-{}",
                crate::device_id::generate()
            ));
            Self {
                delegate: recorder.clone(),
                recorder,
                status: DeviceStatus::default(),
                recent_otps: RecentOtps::default(),
                download_dir: scratch.join("downloads"),
                cache_dir: scratch.join("cache"),
                link: Some(LinkContext {
                    sender: PacketSender(tx),
                    peer: Peer {
                        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                        certificate: peer_identity.certificate_der().as_ref().into(),
                        local: Arc::clone(&local),
                    },
                }),
                sent,
                local,
                peer_identity,
            }
        }

        /// The device's view of us, for the far end of a payload transfer.
        pub(crate) fn device_side_peer(&self) -> Peer {
            Peer {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                certificate: self.local.certificate_der().as_ref().into(),
                local: Arc::clone(&self.peer_identity),
            }
        }

        /// Drops the link, as when the device disconnects.
        pub(crate) fn disconnect(&mut self) {
            self.link = None;
        }

        pub(crate) fn ctx(&mut self) -> Context<'_> {
            Context {
                device_id: Self::DEVICE_ID,
                device_name: Self::DEVICE_NAME,
                delegate: &self.delegate,
                download_dir: &self.download_dir,
                cache_dir: &self.cache_dir,
                link: self.link.clone(),
                status: &mut self.status,
                recent_otps: &mut self.recent_otps,
            }
        }

        /// Packets the plugin has sent since the last call, including ones
        /// sent from tasks holding a clone of the sender.
        pub(crate) fn sent(&mut self) -> Vec<Packet> {
            let mut packets = Vec::new();
            while let Ok(packet) = self.sent.try_recv() {
                packets.push(packet);
            }
            packets
        }

        /// Waits for the next packet the plugin (or one of its tasks) sends.
        pub(crate) async fn next_sent(&mut self) -> Packet {
            tokio::time::timeout(Duration::from_secs(10), self.sent.recv())
                .await
                .expect("timed out waiting for a packet")
                .expect("the link was dropped")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            if let Some(scratch) = self.download_dir.parent() {
                let _ = std::fs::remove_dir_all(scratch);
            }
        }
    }
}
