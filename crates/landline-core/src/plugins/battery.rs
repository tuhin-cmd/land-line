//! Battery: the phone reports its charge. Not implemented yet.

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

pub(crate) const REGISTRATION: Registration = Registration {
    name: "battery",
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Battery),
};

struct Battery;

impl Plugin for Battery {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}
}
