//! Clipboard: text copied on one device offered to the other. Not
//! implemented yet.

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

pub(crate) const REGISTRATION: Registration = Registration {
    name: "clipboard",
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Clipboard),
};

struct Clipboard;

impl Plugin for Clipboard {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}
}
