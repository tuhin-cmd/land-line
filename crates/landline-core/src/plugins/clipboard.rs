//! Clipboard: text copied on one device offered to the other.
//!
//! Two packet types carry it, both with the text in `content`:
//!
//! - `kdeconnect.clipboard` is sent whenever a side's clipboard changes, and
//!   the receiver takes it unconditionally.
//! - `kdeconnect.clipboard.connect` is sent once when a link comes up and
//!   also carries `timestamp`, when that clipboard last changed (ms since
//!   the epoch, 0 for unknown). The receiver takes it only if it is newer
//!   than its own clipboard, so after a reconnect the most recent copy on
//!   either side wins. The phone sends it only if it knows its clipboard,
//!   which on Android 10 and later means it has seen a copy it was allowed
//!   to read.
//!
//! The phone reads its own clipboard only in the foreground (or with
//! `READ_LOGS` granted over ADB), so a copy there reaches us when the user
//! taps "Send clipboard" in the app or its quick-settings tile.
//!
//! The core does not watch the Mac's pasteboard: the app does, and passes
//! each change in as [`Command::SendClipboard`].

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Map;
use tracing::{debug, trace};

use super::{Command, Context, Plugin, Registration};
use crate::packet::Packet;

const TYPE_CLIPBOARD: &str = "kdeconnect.clipboard";
const TYPE_CLIPBOARD_CONNECT: &str = "kdeconnect.clipboard.connect";

/// The most text we take from or offer to a device, in UTF-8 bytes. The
/// apps set no limit of their own beyond the packet size; this keeps a
/// stray copy of a huge log from going to the other side's clipboard.
const MAX_TEXT: usize = 1024 * 1024;

pub(crate) const REGISTRATION: Registration = Registration {
    name: "clipboard",
    incoming: &[TYPE_CLIPBOARD, TYPE_CLIPBOARD_CONNECT],
    // We never send the connect packet; see `Plugin::connected` below.
    outgoing: &[TYPE_CLIPBOARD],
    create: || Box::new(Clipboard::new()),
};

struct Clipboard {
    /// The text last sent to or taken from this device. The app writes what
    /// we pass it to the pasteboard and then reports that change back, and
    /// the phone does the same with what we send; this breaks both loops.
    last: Option<String>,
    /// When this side's clipboard last changed as far as we know, in ms
    /// since the epoch, to judge the device's connect packet. Starts at the
    /// time the plugin is made: the Mac had a clipboard then, and we do not
    /// know how old it is, so only phone copies made since can replace it.
    local_changed_ms: i64,
}

impl Clipboard {
    fn new() -> Self {
        Self {
            last: None,
            local_changed_ms: now_ms(),
        }
    }

    fn take(&mut self, text: String, ctx: &mut Context<'_>) {
        if text.len() > MAX_TEXT {
            debug!(bytes = text.len(), "ignoring an oversized clipboard");
            return;
        }
        self.local_changed_ms = now_ms();
        if self.last.as_deref() == Some(text.as_str()) {
            trace!("clipboard unchanged");
            return;
        }
        trace!(text = %text, "clipboard received");
        ctx.delegate.clipboard_received(ctx.device_id, &text);
        self.last = Some(text);
    }
}

impl Plugin for Clipboard {
    // Nothing is sent on connect. The connect packet exists to hand the
    // other side our clipboard if it is newer than theirs, and the core
    // does not have the Mac's clipboard. A connect packet without
    // `content`, or with a zero timestamp, is ignored by the phone, so
    // sending one would do nothing.
    fn connected(&mut self, _ctx: &mut Context<'_>) {}

    // While apart, either clipboard may have moved on, so the next copy
    // must go through even if it repeats the last one.
    fn disconnected(&mut self, _ctx: &mut Context<'_>) {
        self.last = None;
    }

    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        let Some(text) = packet.body_str("content") else {
            return;
        };
        if packet.kind == TYPE_CLIPBOARD_CONNECT {
            let changed = packet.body_i64("timestamp").unwrap_or(0);
            if changed == 0 || changed <= self.local_changed_ms {
                trace!("device clipboard is not newer than ours");
                return;
            }
        }
        self.take(text, ctx);
    }

    fn command(&mut self, command: Command, ctx: &mut Context<'_>) {
        let Command::SendClipboard { text } = command else {
            return;
        };
        if text.len() > MAX_TEXT {
            debug!(bytes = text.len(), "not sending an oversized clipboard");
            return;
        }
        self.local_changed_ms = now_ms();
        if self.last.as_deref() == Some(text.as_str()) {
            return;
        }
        let mut body = Map::new();
        body.insert("content".into(), text.as_str().into());
        if ctx.send(Packet::new(TYPE_CLIPBOARD, body)) {
            self.last = Some(text);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::plugins::testing::{Event, Harness};

    fn packet(kind: &str, body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("body must be an object")
        };
        Packet::new(kind, body)
    }

    fn sent_texts(harness: &mut Harness) -> Vec<String> {
        harness
            .sent()
            .into_iter()
            .map(|packet| {
                assert_eq!(packet.kind, TYPE_CLIPBOARD);
                packet.body_str("content").unwrap()
            })
            .collect()
    }

    #[test]
    fn device_copies_reach_the_delegate_once() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        let copy = packet(TYPE_CLIPBOARD, json!({"content": "hello"}));
        plugin.packet(&copy, &mut harness.ctx());
        plugin.packet(&copy, &mut harness.ctx());
        assert_eq!(harness.recorder.take(), [Event::Clipboard("hello".into())]);

        // The app writes it to the pasteboard and reports the change back.
        let echo = Command::SendClipboard {
            text: "hello".into(),
        };
        plugin.command(echo, &mut harness.ctx());
        assert!(harness.sent().is_empty());
    }

    #[test]
    fn mac_copies_reach_the_device_once() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();

        for text in ["a", "a", "b"] {
            let command = Command::SendClipboard { text: text.into() };
            plugin.command(command, &mut harness.ctx());
        }
        assert_eq!(sent_texts(&mut harness), ["a", "b"]);

        // The phone sets its clipboard and reports it back.
        plugin.packet(
            &packet(TYPE_CLIPBOARD, json!({"content": "b"})),
            &mut harness.ctx(),
        );
        assert!(harness.recorder.take().is_empty());
    }

    #[test]
    fn a_reconnect_lets_a_repeated_copy_through() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let send = || Command::SendClipboard { text: "x".into() };

        plugin.command(send(), &mut harness.ctx());
        assert_eq!(sent_texts(&mut harness), ["x"]);
        plugin.disconnected(&mut harness.ctx());
        plugin.connected(&mut harness.ctx());
        assert!(harness.sent().is_empty(), "nothing is sent on connect");
        plugin.command(send(), &mut harness.ctx());
        assert_eq!(sent_texts(&mut harness), ["x"]);
    }

    #[test]
    fn connect_packets_win_only_when_newer() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let connect = |text: &str, timestamp: i64| {
            packet(
                TYPE_CLIPBOARD_CONNECT,
                json!({"content": text, "timestamp": timestamp}),
            )
        };

        // Unknown age, and older than this side's clipboard.
        plugin.packet(&connect("old", 0), &mut harness.ctx());
        plugin.packet(&connect("old", now_ms() - 60_000), &mut harness.ctx());
        assert!(harness.recorder.take().is_empty());

        plugin.packet(&connect("new", now_ms() + 60_000), &mut harness.ctx());
        assert_eq!(harness.recorder.take(), [Event::Clipboard("new".into())]);
    }

    #[test]
    fn oversized_text_goes_nowhere() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        let huge = "x".repeat(MAX_TEXT + 1);

        plugin.packet(
            &packet(TYPE_CLIPBOARD, json!({ "content": huge })),
            &mut harness.ctx(),
        );
        plugin.command(Command::SendClipboard { text: huge }, &mut harness.ctx());
        assert!(harness.recorder.take().is_empty());
        assert!(harness.sent().is_empty());
    }
}
