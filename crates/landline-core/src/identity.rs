//! Identity packets: who a device is and what it can do.
//!
//! The same packet type serves three purposes, distinguished only by which
//! optional keys are present: the UDP discovery broadcast (`tcpPort`), the
//! plaintext line written after opening a TCP connection (`targetDeviceId`,
//! `targetProtocolVersion`), and the copy re-sent inside TLS.

use std::ops::RangeInclusive;

use serde_json::{Map, Value};

use crate::device_id;
use crate::packet::{Packet, TYPE_IDENTITY};

/// The protocol version this core speaks.
pub const PROTOCOL_VERSION: i64 = 8;

/// The lowest TCP port a device listens on for links.
pub const MIN_TCP_PORT: u16 = 1716;
/// The highest TCP port a device listens on for links.
pub const MAX_TCP_PORT: u16 = 1764;
/// Every port a device may listen on for links.
pub const TCP_PORT_RANGE: RangeInclusive<u16> = MIN_TCP_PORT..=MAX_TCP_PORT;

/// Whether `port` is one peers will connect to for a link.
pub fn is_link_port(port: u16) -> bool {
    TCP_PORT_RANGE.contains(&port)
}

/// Peers cap names at this many UTF-16 code units.
const MAX_NAME_UTF16_UNITS: usize = 32;

/// Characters peers strip from device names.
const NAME_INVALID_CHARACTERS: &[char] = &[
    '"', '\'', ',', ';', ':', '.', '!', '?', '(', ')', '[', ']', '<', '>',
];

/// What kind of device a peer says it is. Only used for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceType {
    Phone,
    Tablet,
    Desktop,
    Laptop,
    Tv,
}

impl DeviceType {
    /// The wire form, as used in `deviceType`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Tv => "tv",
        }
    }

    /// Reads the wire form. Unknown values become [`DeviceType::Desktop`],
    /// as the KDE Connect apps do, so new types from newer peers still link.
    pub fn parse(value: &str) -> Self {
        match value {
            "phone" => Self::Phone,
            "tablet" => Self::Tablet,
            "laptop" => Self::Laptop,
            "tv" => Self::Tv,
            _ => Self::Desktop,
        }
    }
}

/// Why an identity packet was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("expected a `{TYPE_IDENTITY}` packet, got `{0}`")]
    WrongType(String),
    #[error("identity has a missing or malformed `deviceId`")]
    InvalidDeviceId,
    #[error("identity has no usable `deviceName`")]
    InvalidDeviceName,
    #[error("identity has no `protocolVersion`")]
    MissingProtocolVersion,
    #[error("identity has a `tcpPort` that is not a port number")]
    InvalidTcpPort,
}

/// The contents of a `kdeconnect.identity` packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Valid per [`device_id::is_valid`].
    pub device_id: String,
    /// Already sanitized by [`sanitize_device_name`], and never blank.
    pub device_name: String,
    pub device_type: DeviceType,
    pub protocol_version: i64,
    /// Packet types the device accepts. Peers send nothing else.
    pub incoming_capabilities: Vec<String>,
    /// Packet types the device may send.
    pub outgoing_capabilities: Vec<String>,
    /// Where the device listens for links; set in discovery broadcasts.
    /// Not range-checked here: see [`is_link_port`].
    pub tcp_port: Option<u16>,
    /// The device a TCP connection was opened for; set on the plaintext
    /// identity written after connecting, so a stale address is noticed.
    pub target_device_id: Option<String>,
    /// The protocol version the connecting side expects the other to speak.
    pub target_protocol_version: Option<i64>,
}

impl Identity {
    /// Reads and validates an identity packet from a peer.
    pub fn from_packet(packet: &Packet) -> Result<Self, IdentityError> {
        if packet.kind != TYPE_IDENTITY {
            return Err(IdentityError::WrongType(packet.kind.clone()));
        }

        let device_id = packet
            .body_str("deviceId")
            .filter(|id| device_id::is_valid(id))
            .ok_or(IdentityError::InvalidDeviceId)?;
        let device_name = packet
            .body_str("deviceName")
            .map(|name| sanitize_device_name(&name))
            .filter(|name| !name.is_empty())
            .ok_or(IdentityError::InvalidDeviceName)?;
        let device_type = packet
            .body_str("deviceType")
            .map_or(DeviceType::Desktop, |kind| DeviceType::parse(&kind));
        let protocol_version = packet
            .body_i64("protocolVersion")
            .ok_or(IdentityError::MissingProtocolVersion)?;
        let tcp_port = match packet.body.get("tcpPort") {
            None | Some(Value::Null) => None,
            Some(_) => Some(
                packet
                    .body_i64("tcpPort")
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or(IdentityError::InvalidTcpPort)?,
            ),
        };

        Ok(Self {
            device_id,
            device_name,
            device_type,
            protocol_version,
            incoming_capabilities: packet
                .body_string_list("incomingCapabilities")
                .unwrap_or_default(),
            outgoing_capabilities: packet
                .body_string_list("outgoingCapabilities")
                .unwrap_or_default(),
            tcp_port,
            target_device_id: packet.body_str("targetDeviceId"),
            // The Android app writes this one as a string.
            target_protocol_version: packet.body_i64("targetProtocolVersion"),
        })
    }

    /// The identity as a packet to send, stamped with the current time.
    pub fn to_packet(&self) -> Packet {
        let mut body = Map::new();
        body.insert("deviceId".into(), self.device_id.clone().into());
        body.insert("deviceName".into(), self.device_name.clone().into());
        body.insert("deviceType".into(), self.device_type.as_str().into());
        body.insert("protocolVersion".into(), self.protocol_version.into());
        body.insert(
            "incomingCapabilities".into(),
            self.incoming_capabilities.clone().into(),
        );
        body.insert(
            "outgoingCapabilities".into(),
            self.outgoing_capabilities.clone().into(),
        );
        if let Some(port) = self.tcp_port {
            body.insert("tcpPort".into(), port.into());
        }
        if let Some(target) = &self.target_device_id {
            body.insert("targetDeviceId".into(), target.clone().into());
        }
        if let Some(version) = self.target_protocol_version {
            body.insert("targetProtocolVersion".into(), version.into());
        }
        Packet::new(TYPE_IDENTITY, body)
    }
}

/// Cleans a device name the way peers do: drops the characters
/// `"',;:.!?()[]<>`, trims whitespace, and caps the length. Peers apply the
/// same filter to names they receive, so our own name must pass through it
/// too or it would display differently on each side.
///
/// The cap counts UTF-16 code units, as the Android and desktop apps do, but
/// never splits a character; the result therefore survives their filter
/// unchanged.
pub fn sanitize_device_name(name: &str) -> String {
    let filtered: String = name
        .chars()
        .filter(|c| !NAME_INVALID_CHARACTERS.contains(c))
        .collect();
    let mut capped = String::new();
    let mut units = 0;
    for c in filtered.trim().chars() {
        units += c.len_utf16();
        if units > MAX_NAME_UTF16_UNITS {
            break;
        }
        capped.push(c);
    }
    // The cap may land just after a space.
    capped.truncate(capped.trim_end().len());
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PHONE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const MAC_ID: &str = "aaaabbbb_cccc_dddd_eeee_ffff00001111";

    fn identity_packet(body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("test body must be an object")
        };
        Packet::new(TYPE_IDENTITY, body)
    }

    fn minimal() -> Value {
        json!({"deviceId": PHONE_ID, "deviceName": "Test Phone", "protocolVersion": 8})
    }

    fn with(key: &str, value: Value) -> Packet {
        let mut body = minimal();
        body[key] = value;
        identity_packet(body)
    }

    fn without(key: &str) -> Packet {
        let mut body = minimal();
        body.as_object_mut().unwrap().remove(key);
        identity_packet(body)
    }

    fn sample() -> Identity {
        Identity {
            device_id: MAC_ID.into(),
            device_name: "Test Mac".into(),
            device_type: DeviceType::Laptop,
            protocol_version: PROTOCOL_VERSION,
            incoming_capabilities: vec!["kdeconnect.ping".into()],
            outgoing_capabilities: vec!["kdeconnect.ping".into(), "kdeconnect.clipboard".into()],
            tcp_port: Some(1716),
            target_device_id: Some(PHONE_ID.into()),
            target_protocol_version: Some(8),
        }
    }

    #[test]
    fn device_type_round_trips_and_defaults_to_desktop() {
        for kind in [
            DeviceType::Phone,
            DeviceType::Tablet,
            DeviceType::Desktop,
            DeviceType::Laptop,
            DeviceType::Tv,
        ] {
            assert_eq!(DeviceType::parse(kind.as_str()), kind);
        }
        assert_eq!(DeviceType::parse("toaster"), DeviceType::Desktop);
        assert_eq!(DeviceType::parse("Phone"), DeviceType::Desktop);
    }

    #[test]
    fn link_port_range() {
        assert!(is_link_port(1716) && is_link_port(1764));
        assert!(!is_link_port(1715) && !is_link_port(1765));
    }

    #[test]
    fn minimal_identity_gets_defaults() {
        let identity = Identity::from_packet(&identity_packet(minimal())).unwrap();
        assert_eq!(identity.device_id, PHONE_ID);
        assert_eq!(identity.device_name, "Test Phone");
        assert_eq!(identity.device_type, DeviceType::Desktop);
        assert_eq!(identity.protocol_version, 8);
        assert!(identity.incoming_capabilities.is_empty());
        assert!(identity.outgoing_capabilities.is_empty());
        assert_eq!(identity.tcp_port, None);
        assert_eq!(identity.target_device_id, None);
        assert_eq!(identity.target_protocol_version, None);
    }

    #[test]
    fn round_trips_through_a_packet_line() {
        let identity = sample();
        let line = identity.to_packet().to_line();
        let parsed = Identity::from_packet(&Packet::parse(&line).unwrap()).unwrap();
        assert_eq!(parsed, identity);
    }

    #[test]
    fn to_packet_writes_numbers_and_omits_unset_keys() {
        let packet = sample().to_packet();
        assert_eq!(packet.kind, TYPE_IDENTITY);
        assert_eq!(packet.body["protocolVersion"], json!(8));
        assert_eq!(packet.body["tcpPort"], json!(1716));
        assert_eq!(packet.body["targetProtocolVersion"], json!(8));

        let bare = Identity {
            tcp_port: None,
            target_device_id: None,
            target_protocol_version: None,
            ..sample()
        }
        .to_packet();
        for key in ["tcpPort", "targetDeviceId", "targetProtocolVersion"] {
            assert!(!bare.has(key), "{key} should be absent");
        }
        assert_eq!(
            bare.body["incomingCapabilities"],
            json!(["kdeconnect.ping"])
        );
    }

    #[test]
    fn rejects_other_packet_types() {
        let packet = Packet::new("kdeconnect.ping", Map::new());
        assert_eq!(
            Identity::from_packet(&packet),
            Err(IdentityError::WrongType("kdeconnect.ping".into()))
        );
    }

    #[test]
    fn requires_a_valid_device_id() {
        for bad in [json!("too-short"), json!(null), json!(12345)] {
            assert_eq!(
                Identity::from_packet(&with("deviceId", bad)),
                Err(IdentityError::InvalidDeviceId)
            );
        }
        assert_eq!(
            Identity::from_packet(&without("deviceId")),
            Err(IdentityError::InvalidDeviceId)
        );
    }

    #[test]
    fn requires_a_name_that_survives_sanitizing() {
        for bad in [json!(""), json!("  "), json!("?!.()"), json!(null)] {
            assert_eq!(
                Identity::from_packet(&with("deviceName", bad)),
                Err(IdentityError::InvalidDeviceName)
            );
        }
        assert_eq!(
            Identity::from_packet(&without("deviceName")),
            Err(IdentityError::InvalidDeviceName)
        );
        let identity = Identity::from_packet(&with("deviceName", json!(" Pixel (Work) "))).unwrap();
        assert_eq!(identity.device_name, "Pixel Work");
    }

    #[test]
    fn protocol_version_is_required_and_lenient() {
        assert_eq!(
            Identity::from_packet(&without("protocolVersion")),
            Err(IdentityError::MissingProtocolVersion)
        );
        assert_eq!(
            Identity::from_packet(&with("protocolVersion", json!("abc"))),
            Err(IdentityError::MissingProtocolVersion)
        );
        let identity = Identity::from_packet(&with("protocolVersion", json!("7"))).unwrap();
        assert_eq!(identity.protocol_version, 7);
    }

    #[test]
    fn tcp_port_must_be_a_port_number_when_present() {
        let port =
            |value: Value| Identity::from_packet(&with("tcpPort", value)).map(|i| i.tcp_port);
        assert_eq!(port(json!(1716)), Ok(Some(1716)));
        assert_eq!(port(json!("1739")), Ok(Some(1739)));
        assert_eq!(port(json!(null)), Ok(None));
        // Out of the link range but a real port: the caller decides.
        assert_eq!(port(json!(80)), Ok(Some(80)));
        for bad in [
            json!(-1),
            json!(65536),
            json!("x"),
            json!(true),
            json!([1716]),
        ] {
            assert_eq!(port(bad), Err(IdentityError::InvalidTcpPort));
        }
    }

    #[test]
    fn reads_target_fields_leniently() {
        let mut body = minimal();
        body["targetDeviceId"] = json!(MAC_ID);
        body["targetProtocolVersion"] = json!("8");
        let identity = Identity::from_packet(&identity_packet(body)).unwrap();
        assert_eq!(identity.target_device_id.as_deref(), Some(MAC_ID));
        assert_eq!(identity.target_protocol_version, Some(8));
    }

    #[test]
    fn reads_capabilities_and_type() {
        let mut body = minimal();
        body["deviceType"] = json!("phone");
        body["incomingCapabilities"] = json!(["kdeconnect.ping", "kdeconnect.battery"]);
        body["outgoingCapabilities"] = json!("not a list");
        let identity = Identity::from_packet(&identity_packet(body)).unwrap();
        assert_eq!(identity.device_type, DeviceType::Phone);
        assert_eq!(
            identity.incoming_capabilities,
            ["kdeconnect.ping", "kdeconnect.battery"]
        );
        assert!(identity.outgoing_capabilities.is_empty());
    }

    #[test]
    fn sanitize_removes_punctuation_and_trims() {
        assert_eq!(
            sanitize_device_name(r#"My "Mac": Pro! [2]"#),
            "My Mac Pro 2"
        );
        assert_eq!(sanitize_device_name("  spaced  "), "spaced");
        assert_eq!(sanitize_device_name("Phone-123_test"), "Phone-123_test");
        assert_eq!(sanitize_device_name("<>"), "");
    }

    #[test]
    fn sanitize_caps_at_32_utf16_units_without_splitting() {
        let long = "a".repeat(40);
        assert_eq!(sanitize_device_name(&long), "a".repeat(32));

        // Each emoji is two UTF-16 units: 15 of them plus "ab" is 32 units.
        let emoji = format!("{}abc", "😀".repeat(15));
        assert_eq!(
            sanitize_device_name(&emoji),
            format!("{}ab", "😀".repeat(15))
        );
        // An emoji that would straddle the cap is dropped whole.
        let straddle = format!("{}😀", "a".repeat(31));
        assert_eq!(sanitize_device_name(&straddle), "a".repeat(31));

        // A space landing at the cap is not kept.
        let spaced = format!("{} b", "a".repeat(31));
        assert_eq!(sanitize_device_name(&spaced), "a".repeat(31));
    }

    #[test]
    fn sanitize_is_idempotent() {
        for name in ["  Test (Mac) ", &"x y".repeat(20), "😀😀 :) ok"] {
            let once = sanitize_device_name(name);
            assert_eq!(sanitize_device_name(&once), once);
        }
    }
}
