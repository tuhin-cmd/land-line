//! Notifications: the phone's notifications mirrored to the Mac. Not
//! implemented yet.

use std::path::PathBuf;

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

/// A notification showing on the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneNotification {
    /// Identifies the notification on its device. A later notification with
    /// the same ID replaces this one.
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    /// When the phone posted it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Whether [`crate::Core::dismiss_notification`] can remove it.
    pub dismissable: bool,
    /// The notification was already showing on the phone when we asked for
    /// the current list, so it should be listed but not announced.
    pub silent: bool,
    /// The app's icon as a PNG file in the core's cache, if the phone sent
    /// one.
    pub icon_path: Option<PathBuf>,
}

pub(crate) const REGISTRATION: Registration = Registration {
    name: "notifications",
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Notifications),
};

struct Notifications;

impl Plugin for Notifications {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}
}
