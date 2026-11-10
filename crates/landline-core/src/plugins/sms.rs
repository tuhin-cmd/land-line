//! SMS: text messages as they arrive on the phone. Not implemented yet.

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

/// A text message the phone received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsMessage {
    /// The sender's number or alphanumeric sender ID.
    pub sender: String,
    pub body: String,
    /// When the phone received it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
}

pub(crate) const REGISTRATION: Registration = Registration {
    name: "sms",
    incoming: &[],
    outgoing: &[],
    create: || Box::new(Sms),
};

struct Sms;

impl Plugin for Sms {
    fn packet(&mut self, _packet: &Packet, _ctx: &mut Context<'_>) {}
}
