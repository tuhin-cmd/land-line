//! KDE Connect network packets: one JSON object per line.
//!
//! ```text
//! {"id":1700000000000,"type":"kdeconnect.ping","body":{}}\n
//! ```
//!
//! Parsing is deliberately lenient, because the peers we talk to are: older
//! apps send `id` as a string, bodies mix numbers and numeric strings, and
//! unknown top-level keys appear. The typed body getters follow the coercions
//! the Android app applies when it reads a value, so a packet it would accept
//! reads the same here.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};

/// Sent in discovery and at the start of every link.
pub const TYPE_IDENTITY: &str = "kdeconnect.identity";
/// Pair requests, accepts, rejections and unpairs.
pub const TYPE_PAIR: &str = "kdeconnect.pair";
/// An empty (or `message`-carrying) health check.
pub const TYPE_PING: &str = "kdeconnect.ping";

/// Why a line could not be read as a packet.
#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("packet is not valid JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("packet is not a JSON object")]
    NotAnObject,
    #[error("packet has no `type`, or it is not a non-empty string")]
    MissingType,
    #[error("packet has no `body`, or it is not an object")]
    MissingBody,
    #[error("packet `id` is neither a number nor a numeric string")]
    InvalidId,
    #[error("packet `payloadSize` is not an integer")]
    InvalidPayloadSize,
    #[error("packet `payloadTransferInfo` is not an object")]
    InvalidPayloadTransferInfo,
    #[error("packet has `payloadSize` but no `payloadTransferInfo`")]
    PayloadWithoutTransferInfo,
}

/// One protocol packet.
///
/// `Debug` prints the type, id and body *keys* only: bodies carry
/// notification text, SMS and clipboard contents, which must not reach logs.
#[derive(Clone, PartialEq, Serialize)]
pub struct Packet {
    /// Milliseconds since the Unix epoch when the packet was made. Peers do
    /// not rely on it, and a missing `id` reads as 0.
    pub id: i64,
    /// The packet type, such as [`TYPE_PING`]. Serialized as `type`.
    #[serde(rename = "type")]
    pub kind: String,
    pub body: Map<String, Value>,
    /// Size in bytes of the payload that follows on a separate connection;
    /// -1 means unknown. Never written when `None`, because peers test for
    /// the key's presence rather than its value.
    #[serde(rename = "payloadSize", skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<i64>,
    /// Where to fetch the payload, such as `{"port": 1739}`. Never written
    /// when `None`.
    #[serde(
        rename = "payloadTransferInfo",
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_transfer_info: Option<Map<String, Value>>,
}

impl Packet {
    /// A packet without payload, stamped with the current time.
    pub fn new(kind: impl Into<String>, body: Map<String, Value>) -> Self {
        Self {
            id: now_millis(),
            kind: kind.into(),
            body,
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// The packet as it goes on the wire: compact JSON and exactly one `\n`.
    pub fn to_line(&self) -> String {
        // A struct of strings, integers and JSON values cannot fail to
        // serialize, and the compact form never contains a raw newline.
        let mut line = serde_json::to_string(self).expect("a packet always serializes to JSON");
        line.push('\n');
        line
    }

    /// Reads one line received from a peer. Surrounding whitespace, including
    /// the line terminator, is ignored.
    pub fn parse(line: &str) -> Result<Self, PacketError> {
        let value: Value = serde_json::from_str(line.trim()).map_err(PacketError::Json)?;
        let Value::Object(mut object) = value else {
            return Err(PacketError::NotAnObject);
        };

        let id = match object.get("id") {
            None | Some(Value::Null) => 0,
            Some(value) => lenient_i64(value).ok_or(PacketError::InvalidId)?,
        };
        let kind = match object.remove("type") {
            Some(Value::String(kind)) if !kind.is_empty() => kind,
            _ => return Err(PacketError::MissingType),
        };
        let body = match object.remove("body") {
            Some(Value::Object(body)) => body,
            _ => return Err(PacketError::MissingBody),
        };
        let payload_size = match object.get("payloadSize") {
            None | Some(Value::Null) => None,
            Some(value) => Some(lenient_i64(value).ok_or(PacketError::InvalidPayloadSize)?),
        };
        let payload_transfer_info = match object.remove("payloadTransferInfo") {
            None | Some(Value::Null) => None,
            Some(Value::Object(info)) => Some(info),
            Some(_) => return Err(PacketError::InvalidPayloadTransferInfo),
        };
        // The Android app refuses such packets: it would have nowhere to
        // fetch the announced bytes from.
        if payload_size.is_some() && payload_transfer_info.is_none() {
            return Err(PacketError::PayloadWithoutTransferInfo);
        }

        Ok(Self {
            id,
            kind,
            body,
            payload_size,
            payload_transfer_info,
        })
    }

    /// Whether the body has `key`, even if its value is null.
    pub fn has(&self, key: &str) -> bool {
        self.body.contains_key(key)
    }

    /// A body value as a string. Numbers and booleans are rendered as text;
    /// arrays and objects give `None`.
    pub fn body_str(&self, key: &str) -> Option<String> {
        primitive_string(self.body.get(key)?)
    }

    /// A body value as an integer: a JSON number (fractions truncated toward
    /// zero, out-of-range values saturated) or a string holding one.
    pub fn body_i64(&self, key: &str) -> Option<i64> {
        lenient_i64(self.body.get(key)?)
    }

    /// A body value as a boolean: a JSON boolean or the string `true` or
    /// `false` in any case. Anything else, including numbers, gives `None`.
    pub fn body_bool(&self, key: &str) -> Option<bool> {
        match self.body.get(key)? {
            Value::Bool(value) => Some(*value),
            Value::String(text) if text.eq_ignore_ascii_case("true") => Some(true),
            Value::String(text) if text.eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        }
    }

    /// A body array as strings. Primitive elements are rendered as text, and
    /// null, array and object elements are skipped. `None` if the value is
    /// missing or not an array.
    pub fn body_string_list(&self, key: &str) -> Option<Vec<String>> {
        match self.body.get(key)? {
            Value::Array(items) => Some(items.iter().filter_map(primitive_string).collect()),
            _ => None,
        }
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packet")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("body_keys", &self.body.keys().collect::<Vec<_>>())
            .field("payload_size", &self.payload_size)
            .field("payload_transfer_info", &self.payload_transfer_info)
            .finish()
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn primitive_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn lenient_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(truncate)),
        Value::String(text) => text.parse::<i64>().ok().or_else(|| {
            text.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(truncate)
        }),
        _ => None,
    }
}

// `as` truncates toward zero and saturates at the i64 bounds, which is what
// the Android app's Double-to-Long conversion does too.
fn truncate(value: f64) -> i64 {
    value as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("test body must be an object"),
        }
    }

    fn parse_body(body_json: &str) -> Packet {
        Packet::parse(&format!(r#"{{"id":1,"type":"t","body":{body_json}}}"#)).unwrap()
    }

    #[test]
    fn new_stamps_the_current_time() {
        let before = now_millis();
        let packet = Packet::new(TYPE_PING, Map::new());
        assert!(packet.id >= before && packet.id <= now_millis());
        assert_eq!(packet.kind, TYPE_PING);
    }

    #[test]
    fn to_line_is_compact_with_one_newline_and_no_payload_keys() {
        let mut packet = Packet::new(TYPE_PING, body(json!({"message": "a\nb"})));
        packet.id = 42;
        let line = packet.to_line();
        assert_eq!(
            line,
            "{\"id\":42,\"type\":\"kdeconnect.ping\",\"body\":{\"message\":\"a\\nb\"}}\n"
        );
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn to_line_writes_payload_keys_when_set() {
        let mut packet = Packet::new("kdeconnect.share.request", Map::new());
        packet.payload_size = Some(-1);
        packet.payload_transfer_info = Some(body(json!({"port": 1739})));
        let line = packet.to_line();
        assert!(line.contains(r#""payloadSize":-1"#));
        assert!(line.contains(r#""payloadTransferInfo":{"port":1739}"#));
    }

    #[test]
    fn round_trips() {
        let mut packet = Packet::new("t", body(json!({"a": [1, "b"], "c": {"d": null}})));
        packet.payload_size = Some(42);
        packet.payload_transfer_info = Some(body(json!({"port": 1740})));
        assert_eq!(Packet::parse(&packet.to_line()).unwrap(), packet);
    }

    #[test]
    fn parse_tolerates_whitespace_and_crlf() {
        let packet = Packet::parse(" {\"id\":5,\"type\":\"t\",\"body\":{}}\r\n ").unwrap();
        assert_eq!(packet.id, 5);
    }

    #[test]
    fn parse_accepts_string_missing_and_null_ids() {
        let id = |line: &str| Packet::parse(line).unwrap().id;
        assert_eq!(
            id(r#"{"id":"1700000000000","type":"t","body":{}}"#),
            1_700_000_000_000
        );
        assert_eq!(id(r#"{"type":"t","body":{}}"#), 0);
        assert_eq!(id(r#"{"id":null,"type":"t","body":{}}"#), 0);
        assert_eq!(id(r#"{"id":12.9,"type":"t","body":{}}"#), 12);
        assert!(matches!(
            Packet::parse(r#"{"id":"soon","type":"t","body":{}}"#),
            Err(PacketError::InvalidId)
        ));
    }

    #[test]
    fn parse_ignores_unknown_top_level_keys() {
        let packet = Packet::parse(r#"{"id":1,"type":"t","body":{},"extra":0}"#).unwrap();
        assert_eq!(packet.kind, "t");
        assert!(packet.payload_size.is_none());
    }

    #[test]
    fn parse_requires_type_and_body() {
        let err = |line: &str| Packet::parse(line).unwrap_err();
        assert!(matches!(err(r#"{"body":{}}"#), PacketError::MissingType));
        assert!(matches!(
            err(r#"{"type":"","body":{}}"#),
            PacketError::MissingType
        ));
        assert!(matches!(
            err(r#"{"type":7,"body":{}}"#),
            PacketError::MissingType
        ));
        assert!(matches!(err(r#"{"type":"t"}"#), PacketError::MissingBody));
        assert!(matches!(
            err(r#"{"type":"t","body":[]}"#),
            PacketError::MissingBody
        ));
        assert!(matches!(err("[]"), PacketError::NotAnObject));
        assert!(matches!(err("{"), PacketError::Json(_)));
    }

    #[test]
    fn parse_rejects_payload_size_without_transfer_info() {
        assert!(matches!(
            Packet::parse(r#"{"id":1,"type":"t","body":{},"payloadSize":42}"#),
            Err(PacketError::PayloadWithoutTransferInfo)
        ));
        // Transfer info alone is harmless.
        let packet =
            Packet::parse(r#"{"id":1,"type":"t","body":{},"payloadTransferInfo":{"port":1739}}"#)
                .unwrap();
        assert!(packet.payload_size.is_none());
        assert!(packet.payload_transfer_info.is_some());
    }

    #[test]
    fn parse_reads_payload_fields() {
        let packet = Packet::parse(
            r#"{"id":1,"type":"t","body":{},"payloadSize":"-1","payloadTransferInfo":{"port":1739}}"#,
        )
        .unwrap();
        assert_eq!(packet.payload_size, Some(-1));
        assert_eq!(packet.payload_transfer_info.unwrap()["port"], json!(1739));
        assert!(matches!(
            Packet::parse(r#"{"type":"t","body":{},"payloadSize":true,"payloadTransferInfo":{}}"#),
            Err(PacketError::InvalidPayloadSize)
        ));
        assert!(matches!(
            Packet::parse(r#"{"type":"t","body":{},"payloadSize":1,"payloadTransferInfo":5}"#),
            Err(PacketError::InvalidPayloadTransferInfo)
        ));
    }

    #[test]
    fn body_str_renders_primitives() {
        let packet = parse_body(r#"{"s":"x","n":8,"f":1.5,"b":true,"nil":null,"a":[1],"o":{}}"#);
        assert_eq!(packet.body_str("s").as_deref(), Some("x"));
        assert_eq!(packet.body_str("n").as_deref(), Some("8"));
        assert_eq!(packet.body_str("f").as_deref(), Some("1.5"));
        assert_eq!(packet.body_str("b").as_deref(), Some("true"));
        assert_eq!(packet.body_str("nil"), None);
        assert_eq!(packet.body_str("a"), None);
        assert_eq!(packet.body_str("o"), None);
        assert_eq!(packet.body_str("missing"), None);
    }

    #[test]
    fn body_i64_coerces_like_the_android_app() {
        let packet = parse_body(
            r#"{"n":8,"s":"12","f":2.9,"neg":-2.9,"sf":"3.7","big":1e30,"bad":"x","b":true,"nil":null,"inf":"Infinity"}"#,
        );
        assert_eq!(packet.body_i64("n"), Some(8));
        assert_eq!(packet.body_i64("s"), Some(12));
        assert_eq!(packet.body_i64("f"), Some(2));
        assert_eq!(packet.body_i64("neg"), Some(-2));
        assert_eq!(packet.body_i64("sf"), Some(3));
        assert_eq!(packet.body_i64("big"), Some(i64::MAX));
        assert_eq!(packet.body_i64("bad"), None);
        assert_eq!(packet.body_i64("b"), None);
        assert_eq!(packet.body_i64("nil"), None);
        assert_eq!(packet.body_i64("inf"), None);
        assert_eq!(packet.body_i64("missing"), None);
    }

    #[test]
    fn body_bool_accepts_booleans_and_their_strings() {
        let packet = parse_body(
            r#"{"t":true,"f":false,"st":"TRUE","sf":"False","one":1,"x":"yes","nil":null}"#,
        );
        assert_eq!(packet.body_bool("t"), Some(true));
        assert_eq!(packet.body_bool("f"), Some(false));
        assert_eq!(packet.body_bool("st"), Some(true));
        assert_eq!(packet.body_bool("sf"), Some(false));
        assert_eq!(packet.body_bool("one"), None);
        assert_eq!(packet.body_bool("x"), None);
        assert_eq!(packet.body_bool("nil"), None);
        assert_eq!(packet.body_bool("missing"), None);
    }

    #[test]
    fn body_string_list_renders_primitive_elements() {
        let packet = parse_body(r#"{"l":["a",1,true,null,[2],{}],"e":[],"s":"a","nil":null}"#);
        assert_eq!(
            packet.body_string_list("l"),
            Some(vec!["a".to_owned(), "1".to_owned(), "true".to_owned()])
        );
        assert_eq!(packet.body_string_list("e"), Some(vec![]));
        assert_eq!(packet.body_string_list("s"), None);
        assert_eq!(packet.body_string_list("nil"), None);
        assert_eq!(packet.body_string_list("missing"), None);
    }

    #[test]
    fn has_sees_null_values() {
        let packet = parse_body(r#"{"nil":null}"#);
        assert!(packet.has("nil"));
        assert!(!packet.has("missing"));
    }

    #[test]
    fn debug_shows_keys_but_not_values() {
        let packet = Packet::new(
            "kdeconnect.clipboard",
            body(json!({"content": "secret-clip-value"})),
        );
        let printed = format!("{packet:?}");
        assert!(printed.contains("kdeconnect.clipboard"));
        assert!(printed.contains("content"));
        assert!(!printed.contains("secret-clip-value"));
    }
}
