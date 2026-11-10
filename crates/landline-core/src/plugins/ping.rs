//! Ping: a packet with nothing in it, to check the link by hand.

use serde_json::Map;

use super::{Command, Context, Plugin, Registration};
use crate::packet::{Packet, TYPE_PING};

pub(crate) const REGISTRATION: Registration = Registration {
    name: "ping",
    incoming: &[TYPE_PING],
    outgoing: &[TYPE_PING],
    create: || Box::new(Ping),
};

struct Ping;

impl Plugin for Ping {
    fn packet(&mut self, _packet: &Packet, ctx: &mut Context<'_>) {
        ctx.delegate.ping_received(ctx.device_id);
    }

    fn command(&mut self, command: Command, ctx: &mut Context<'_>) {
        if let Command::Ping = command {
            ctx.send(Packet::new(TYPE_PING, Map::new()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::testing::{Event, Harness};

    #[test]
    fn pings_go_both_ways() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        plugin.packet(&Packet::new(TYPE_PING, Map::new()), &mut harness.ctx());
        assert_eq!(harness.recorder.take(), [Event::Ping]);

        plugin.command(Command::Ping, &mut harness.ctx());
        let sent = harness.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, TYPE_PING);
    }
}
