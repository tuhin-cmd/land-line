//! Battery: the phone reports its charge.
//!
//! The Android app (`plugins/battery/BatteryPlugin.kt`) sends
//! `kdeconnect.battery` with `currentCharge` (percent), `isCharging` and
//! `thresholdEvent` (1 when the phone crosses its low-battery threshold)
//! as soon as its plugin starts for us, which is when the link comes up,
//! and again whenever one of those values changes. There is no request
//! packet, so there is nothing to send.

use super::{Context, Plugin, Registration};
use crate::packet::Packet;

pub(crate) const TYPE_BATTERY: &str = "kdeconnect.battery";

pub(crate) const REGISTRATION: Registration = Registration {
    name: "battery",
    incoming: &[TYPE_BATTERY],
    outgoing: &[],
    create: || Box::new(BatteryPlugin),
};

struct BatteryPlugin;

impl Plugin for BatteryPlugin {
    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        ctx.status.battery = read(packet);
    }
}

/// The battery state in `packet`. A missing or negative charge is how the
/// desktop app says it has no battery, so it clears the state rather than
/// keeping a stale one.
fn read(packet: &Packet) -> Option<crate::Battery> {
    let charge = packet.body_i64("currentCharge")?;
    if charge < 0 {
        return None;
    }
    // The phone computes level * 100 / scale, which a misreporting battery
    // driver can push past 100.
    let charge = u8::try_from(charge.min(100)).ok()?;
    Some(crate::Battery {
        charge,
        charging: packet.body_bool("isCharging").unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::Battery;
    use crate::plugins::testing::Harness;

    fn battery_packet(body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("test body must be an object");
        };
        Packet::new(TYPE_BATTERY, body)
    }

    fn receive(harness: &mut Harness, body: Value) -> Option<Battery> {
        let mut plugin = (REGISTRATION.create)();
        plugin.packet(&battery_packet(body), &mut harness.ctx());
        harness.status.battery
    }

    #[test]
    fn registers_only_the_battery_packet_as_incoming() {
        assert_eq!(REGISTRATION.incoming, [TYPE_BATTERY]);
        assert!(REGISTRATION.outgoing.is_empty());
    }

    #[test]
    fn reads_what_the_android_app_sends() {
        let mut harness = Harness::new();
        let battery = receive(
            &mut harness,
            json!({"currentCharge": 57, "isCharging": true, "thresholdEvent": 0}),
        );
        assert_eq!(
            battery,
            Some(Battery {
                charge: 57,
                charging: true
            })
        );

        let battery = receive(
            &mut harness,
            json!({"currentCharge": 14, "isCharging": false, "thresholdEvent": 1}),
        );
        assert_eq!(
            battery,
            Some(Battery {
                charge: 14,
                charging: false
            })
        );
    }

    #[test]
    fn reads_values_leniently() {
        let mut harness = Harness::new();
        let battery = receive(
            &mut harness,
            json!({"currentCharge": "80", "isCharging": "TRUE"}),
        );
        assert_eq!(
            battery,
            Some(Battery {
                charge: 80,
                charging: true
            })
        );
        let battery = receive(&mut harness, json!({"currentCharge": 33}));
        assert_eq!(
            battery,
            Some(Battery {
                charge: 33,
                charging: false
            })
        );
    }

    #[test]
    fn clamps_an_overfull_charge() {
        let mut harness = Harness::new();
        let battery = receive(&mut harness, json!({"currentCharge": 250}));
        assert_eq!(battery.map(|b| b.charge), Some(100));
    }

    #[test]
    fn a_missing_or_negative_charge_clears_the_state() {
        let mut harness = Harness::new();
        assert!(receive(&mut harness, json!({"currentCharge": 50})).is_some());
        assert_eq!(receive(&mut harness, json!({"currentCharge": -1})), None);

        assert!(receive(&mut harness, json!({"currentCharge": 50})).is_some());
        assert_eq!(receive(&mut harness, json!({"isCharging": true})), None);

        assert!(receive(&mut harness, json!({"currentCharge": 50})).is_some());
        assert_eq!(
            receive(&mut harness, json!({"currentCharge": "full"})),
            None
        );

        let empty = Packet::new(TYPE_BATTERY, Map::new());
        assert_eq!(read(&empty), None);
    }
}
