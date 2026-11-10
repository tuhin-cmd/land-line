//! Share: files, text and links sent between the devices. Not implemented
//! yet.

use std::path::PathBuf;

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// From the device to this computer.
    Incoming,
    /// From this computer to the device.
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    InProgress,
    /// For an incoming file, `path` is where it was saved; for an outgoing
    /// one, the file that was sent.
    Finished {
        path: PathBuf,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

/// One file moving between the devices. Reported through
/// [`crate::CoreDelegate::transfer_updated`] when it starts, a few times a
/// second while it runs, and when it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// Unique for the life of the core.
    pub id: u64,
    pub device_id: String,
    pub file_name: String,
    pub direction: TransferDirection,
    pub bytes_done: u64,
    /// `None` when the sender did not say how large the file is.
    pub bytes_total: Option<u64>,
    pub state: TransferState,
}

pub(crate) const REGISTRATION: Registration = Registration {
    name: "share",
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Share),
};

struct Share;

impl Plugin for Share {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}
}
