//! SMS: text messages as they arrive on the phone.
//!
//! The Android app pushes `kdeconnect.sms.messages` whenever its messages
//! database changes, but only after this side has asked it for messages at
//! least once since its SMS plugin was created (`SMSPlugin.kt`,
//! `haveMessagesBeenRequested`, set in `handleRequestAllConversations` and
//! `handleRequestSingleConversation`). That plugin is created afresh each
//! time the link comes up, so we ask on every connection, with the cheapest
//! request there is: one conversation, for a thread ID that cannot exist
//! (`SMSHelper.ThreadID.invalidThreadId`), which flips the flag and is
//! answered with an empty list. Asking for all conversations would do the
//! same at the cost of one packet per conversation on the phone.
//!
//! What a push contains is not "the new message" but every message dated at
//! or after the newest one the phone had already seen (`sendLatestMessage`,
//! `getMessagesInRange(..., mostRecentTimestamp, ...)`), so the same message
//! comes back on every later change (read flags, sent replies), and the
//! first push after a link comes up carries the newest message from before
//! it. See [`Sms::is_new`] for how those are told apart.
//!
//! A message, as `SMSHelper.Message.toJsonObject` writes it:
//!
//! ```json
//! {"addresses": [{"address": "<sender>"}], "body": "<text>", "date": 1700000000000,
//!  "type": 1, "read": 0, "thread_id": 12, "_id": 345, "sub_id": 1, "event": 1}
//! ```
//!
//! MMS messages also carry `attachments` (a list, possibly empty); their
//! `_id` comes from a different table, so it may equal an SMS's.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use super::{Context, PacketSender, Plugin, Registration};
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

/// Messages from the phone, pushed or in answer to a request.
pub(crate) const TYPE_SMS_MESSAGES: &str = "kdeconnect.sms.messages";
/// Asks for the messages of one conversation.
pub(crate) const TYPE_SMS_REQUEST_CONVERSATION: &str = "kdeconnect.sms.request_conversation";

pub(crate) const REGISTRATION: Registration = Registration {
    name: "sms",
    incoming: &[TYPE_SMS_MESSAGES],
    outgoing: &[TYPE_SMS_REQUEST_CONVERSATION],
    create: || Box::new(Sms::new(Timing::DEFAULT)),
};

/// `Telephony.Sms.MESSAGE_TYPE_INBOX`; MMS inbox messages are mapped to it.
const MESSAGE_TYPE_INBOX: i64 = 1;
/// `SMSHelper.Message.EVENT_TEXT_MESSAGE`: the message has a text body.
const EVENT_TEXT_MESSAGE: i64 = 0x1;

/// A message dated more than this before the packet that carries it is not
/// news: the phone pushes within moments of a message arriving, so an older
/// one is a resend, and a code that old should not reach the clipboard.
const MAX_AGE_MS: i64 = 10 * 60 * 1000;
/// Slack on the link-up time, which is estimated, and on MMS dates, which
/// the phone stores in whole seconds.
const LINK_UP_GRACE_MS: i64 = 2_000;

/// When to ask the phone again.
#[derive(Clone, Copy)]
struct Timing {
    /// First wait after an unanswered request; doubles up to `refresh`.
    retry: Duration,
    /// Wait between requests once one was answered. The phone forgets that
    /// we asked whenever it recreates its SMS plugin while the link stays up
    /// (a permission granted, the plugin toggled), and only a new request
    /// turns pushes back on.
    refresh: Duration,
}

impl Timing {
    const DEFAULT: Self = Self {
        retry: Duration::from_secs(5),
        refresh: Duration::from_secs(5 * 60),
    };
}

/// Identifies a message across pushes: SMS and MMS ids are separate.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MessageKey {
    mms: bool,
    id: i64,
}

struct Sms {
    timing: Timing,
    /// When the current link came up, on this machine's clock.
    link_up: Option<Instant>,
    /// Messages already reported, with their dates, for pruning.
    seen: HashMap<MessageKey, i64>,
    /// Set once the phone sends anything, which means it heard a request.
    answered: Arc<AtomicBool>,
    requester: Option<JoinHandle<()>>,
}

impl Sms {
    fn new(timing: Timing) -> Self {
        Self {
            timing,
            link_up: None,
            seen: HashMap::new(),
            answered: Arc::new(AtomicBool::new(false)),
            requester: None,
        }
    }

    fn stop_requesting(&mut self) {
        if let Some(task) = self.requester.take() {
            task.abort();
        }
    }

    /// Whether `message`, in a packet the phone sent at `sent_ms` on its own
    /// clock, arrived on the phone since the link came up.
    ///
    /// Both sides of each comparison are on the phone's clock: the packet
    /// `id` is the phone's time when it wrote the packet, and the link-up
    /// time is that minus how long ago, by our clock, the link came up. So
    /// a phone whose clock is off by minutes still works, which comparing
    /// message dates with this machine's clock would not.
    ///
    /// - Dated before the link came up: the newest old message, which the
    ///   first push after link-up repeats, or anything an answer to a
    ///   request might carry.
    /// - Dated well before the packet: a resend, see [`MAX_AGE_MS`].
    /// - Already seen: the phone repeats the newest message on every change.
    fn is_new(&self, message: &Parsed, sent_ms: i64, link_up_ms: i64) -> bool {
        message.date >= link_up_ms - LINK_UP_GRACE_MS
            && message.date >= sent_ms - MAX_AGE_MS
            && !self.seen.contains_key(&message.key)
    }
}

impl Drop for Sms {
    fn drop(&mut self) {
        self.stop_requesting();
    }
}

impl Plugin for Sms {
    fn connected(&mut self, ctx: &mut Context<'_>) {
        self.stop_requesting();
        self.link_up = Some(Instant::now());
        self.answered = Arc::new(AtomicBool::new(false));
        ctx.send(request());

        // Without a runtime (plugin tests) there are no retries; the first
        // request has gone out regardless.
        if let (Some(link), Ok(runtime)) = (&ctx.link, tokio::runtime::Handle::try_current()) {
            self.requester = Some(runtime.spawn(keep_requesting(
                link.sender.clone(),
                Arc::clone(&self.answered),
                self.timing,
            )));
        }
    }

    fn disconnected(&mut self, _ctx: &mut Context<'_>) {
        self.stop_requesting();
        self.link_up = None;
    }

    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        if packet.kind != TYPE_SMS_MESSAGES {
            return;
        }
        self.answered.store(true, Ordering::Relaxed);
        let Some(link_up) = self.link_up else {
            return;
        };
        let sent_ms = if packet.id > 0 {
            packet.id
        } else {
            now_millis()
        };
        let since_link_up = i64::try_from(link_up.elapsed().as_millis()).unwrap_or(i64::MAX);
        let link_up_ms = sent_ms.saturating_sub(since_link_up);

        let Some(Value::Array(messages)) = packet.body.get("messages") else {
            tracing::debug!(device = ctx.device_id, "SMS packet without a message list");
            return;
        };
        let mut new: Vec<Parsed> = messages
            .iter()
            .filter_map(Parsed::from_json)
            .filter(|message| message.is_received_text())
            .filter(|message| self.is_new(message, sent_ms, link_up_ms))
            .collect();
        tracing::debug!(
            device = ctx.device_id,
            total = messages.len(),
            new = new.len(),
            "SMS packet"
        );

        // The phone lists newest first; report in the order they arrived.
        new.sort_by_key(|message| message.date);
        for message in new {
            if self.seen.insert(message.key, message.date).is_some() {
                // The same message twice in one packet.
                continue;
            }
            tracing::trace!(sender = %message.sender, body = %message.body, "new SMS");
            let sms = SmsMessage {
                sender: message.sender,
                body: message.body,
                time_ms: message.date,
            };
            ctx.delegate.sms_received(ctx.device_id, &sms);
            ctx.scan_for_otp(&sms.sender, &sms.body);
        }

        // Anything this old fails the age test anyway, so it need not be
        // remembered.
        self.seen.retain(|_, date| *date >= sent_ms - MAX_AGE_MS);
    }
}

/// The request that turns pushes on; see the module documentation.
fn request() -> Packet {
    let Value::Object(body) = json!({
        "threadID": -1,
        "numberToRequest": 1,
    }) else {
        unreachable!("a JSON object literal is an object");
    };
    Packet::new(TYPE_SMS_REQUEST_CONVERSATION, body)
}

/// Asks again until the phone answers, then now and then, until the link
/// is gone. A request that arrives before the phone has loaded its SMS
/// plugin for the new link is dropped there without a trace.
async fn keep_requesting(sender: PacketSender, answered: Arc<AtomicBool>, timing: Timing) {
    let mut retry = timing.retry;
    loop {
        let wait = if answered.load(Ordering::Relaxed) {
            timing.refresh
        } else {
            let wait = retry;
            retry = (retry * 2).min(timing.refresh);
            wait
        };
        tokio::time::sleep(wait).await;
        if !sender.send(request()) {
            return;
        }
    }
}

/// The fields of one message that decide whether and how to report it.
struct Parsed {
    key: MessageKey,
    sender: String,
    body: String,
    date: i64,
    kind: i64,
    event: i64,
}

impl Parsed {
    /// Reads one message leniently: older app versions wrote numbers as
    /// strings (`"date": "1518846484880"`, `"type": "2"`).
    fn from_json(value: &Value) -> Option<Self> {
        let message = value.as_object()?;
        let sender = message
            .get("addresses")
            .and_then(Value::as_array)
            .and_then(|addresses| addresses.first())
            .and_then(|first| first.get("address"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Some(Self {
            key: MessageKey {
                mms: message.contains_key("attachments"),
                id: int(message, "_id")?,
            },
            sender,
            body: message
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            date: int(message, "date")?,
            kind: int(message, "type")?,
            // Messages from before event flags existed were all text.
            event: int(message, "event").unwrap_or(EVENT_TEXT_MESSAGE),
        })
    }

    /// Received (not sent, queued, draft or failed) and carrying text, so
    /// not an MMS of only a picture.
    fn is_received_text(&self) -> bool {
        self.kind == MESSAGE_TYPE_INBOX
            && self.event & EVENT_TEXT_MESSAGE != 0
            && !self.body.trim().is_empty()
    }
}

fn int(object: &Map<String, Value>, key: &str) -> Option<i64> {
    match object.get(key)? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::OtpCode;
    use crate::plugins::testing::{Event, Harness};

    // Invented senders and texts; none are real.
    const SENDER: &str = "+10000000001";
    const SHOP: &str = "ExampleShop";

    fn connected() -> (Harness, Box<dyn Plugin>) {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        plugin.connected(&mut harness.ctx());
        (harness, plugin)
    }

    /// A message as `SMSHelper.Message.toJsonObject` writes it for an SMS.
    fn sms(id: i64, kind: i64, sender: &str, body: &str, date: i64) -> Value {
        json!({
            "addresses": [{"address": sender}],
            "body": body,
            "date": date,
            "type": kind,
            "read": 0,
            "thread_id": 7,
            "_id": id,
            "sub_id": 1,
            "event": EVENT_TEXT_MESSAGE,
        })
    }

    /// A push as `SMSPlugin.constructBulkMessagePacket` builds it, sent by
    /// the phone at `sent_ms` on its clock.
    fn push(sent_ms: i64, messages: Vec<Value>) -> Packet {
        let Value::Object(body) = json!({"version": 2, "messages": messages}) else {
            unreachable!()
        };
        let mut packet = Packet::new(TYPE_SMS_MESSAGES, body);
        packet.id = sent_ms;
        packet
    }

    fn sms_events(events: &[Event]) -> Vec<SmsMessage> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Sms(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn asks_for_a_conversation_that_cannot_exist_on_connect() {
        let (mut harness, _plugin) = connected();
        let sent = harness.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, TYPE_SMS_REQUEST_CONVERSATION);
        assert_eq!(sent[0].body_i64("threadID"), Some(-1));
        assert_eq!(sent[0].body_i64("numberToRequest"), Some(1));
    }

    #[test]
    fn a_new_message_is_reported_once() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        let message = sms(41, 1, SHOP, "Your order has shipped", now);

        plugin.packet(&push(now, vec![message.clone()]), &mut harness.ctx());
        assert_eq!(
            harness.recorder.take(),
            [Event::Sms(SmsMessage {
                sender: SHOP.into(),
                body: "Your order has shipped".into(),
                time_ms: now,
            })]
        );

        // The phone repeats the newest message on every later change, here
        // with the read flag set.
        let mut read = message;
        read["read"] = json!(1);
        plugin.packet(&push(now + 30_000, vec![read]), &mut harness.ctx());
        assert_eq!(harness.recorder.take(), []);
    }

    #[test]
    fn the_message_from_before_the_link_is_not_news() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        // The first push after link-up carries the newest old message, dated
        // a minute before the link came up, and the new one.
        let old = sms(40, 1, SENDER, "See you tomorrow", now - 60_000);
        let new = sms(41, 1, SENDER, "Running late", now);

        plugin.packet(&push(now, vec![new, old]), &mut harness.ctx());
        let reported = sms_events(&harness.recorder.take());
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].body, "Running late");
    }

    #[test]
    fn messages_dated_long_before_the_packet_are_not_news() {
        let (mut harness, mut plugin) = connected();
        // The phone's clock runs an hour ahead of ours; what counts is the
        // message's age by the phone's clock.
        let phone_now = now_millis() + 3_600_000;
        let stale = sms(42, 1, SENDER, "Old news", phone_now - MAX_AGE_MS - 1);
        let fresh = sms(43, 1, SENDER, "Fresh news", phone_now - 1_000);

        plugin.packet(&push(phone_now, vec![fresh, stale]), &mut harness.ctx());
        let reported = sms_events(&harness.recorder.take());
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].body, "Fresh news");
    }

    #[test]
    fn sent_and_textless_messages_are_ignored() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        // Telephony.Sms.MESSAGE_TYPE_SENT, _OUTBOX, _FAILED, _QUEUED.
        let outgoing = [2, 4, 5, 6]
            .into_iter()
            .map(|kind| sms(50 + kind, kind, SENDER, "Your code is 482913", now));
        // An MMS of only a picture: no text event flag, empty body.
        let picture = json!({
            "addresses": [{"address": SENDER}],
            "body": "",
            "date": now,
            "type": 1,
            "read": 0,
            "thread_id": 7,
            "_id": 60,
            "sub_id": 1,
            "event": 0,
            "attachments": [{
                "part_id": 1,
                "mime_type": "image/jpeg",
                "unique_identifier": "PART_1.jpg",
            }],
        });
        let mut messages: Vec<Value> = outgoing.collect();
        messages.push(picture);

        plugin.packet(&push(now, messages), &mut harness.ctx());
        assert_eq!(harness.recorder.take(), []);
    }

    #[test]
    fn an_mms_with_text_is_reported_and_kept_apart_from_an_sms_with_its_id() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        let text = sms(70, 1, SENDER, "First", now);
        let mms = json!({
            "addresses": [{"address": SENDER}, {"address": "+10000000002"}],
            "body": "Second",
            "date": now / 1000 * 1000,
            "type": 1,
            "read": 0,
            "thread_id": 8,
            "_id": 70,
            "sub_id": 1,
            "event": EVENT_TEXT_MESSAGE | 0x2,
            "attachments": [],
        });

        plugin.packet(&push(now, vec![text, mms]), &mut harness.ctx());
        let mut bodies: Vec<String> = sms_events(&harness.recorder.take())
            .into_iter()
            .map(|message| message.body)
            .collect();
        bodies.sort();
        assert_eq!(bodies, ["First", "Second"]);
    }

    #[test]
    fn a_code_is_reported_once_with_the_sender_as_source() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        let message = sms(
            80,
            1,
            SHOP,
            "Your ExampleShop verification code is 482913. Never share it.",
            now,
        );

        plugin.packet(&push(now, vec![message.clone()]), &mut harness.ctx());
        let events = harness.recorder.take();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::Sms(_)));
        assert_eq!(
            events[1],
            Event::Otp(OtpCode {
                code: "482913".into(),
                source: SHOP.into(),
            })
        );

        plugin.packet(&push(now + 5_000, vec![message]), &mut harness.ctx());
        assert_eq!(harness.recorder.take(), []);
    }

    #[test]
    fn numbers_written_as_strings_are_read() {
        let (mut harness, mut plugin) = connected();
        let now = now_millis();
        let message = json!({
            "addresses": [{"address": SENDER}],
            "body": "Hello",
            "date": now.to_string(),
            "type": "1",
            "thread_id": "7",
            "_id": "90",
        });

        plugin.packet(&push(now, vec![message]), &mut harness.ctx());
        assert_eq!(sms_events(&harness.recorder.take()).len(), 1);
    }

    #[test]
    fn the_empty_answer_to_our_request_reports_nothing() {
        let (mut harness, mut plugin) = connected();
        plugin.packet(&push(now_millis(), vec![]), &mut harness.ctx());
        plugin.packet(
            &Packet::new(TYPE_SMS_MESSAGES, Map::new()),
            &mut harness.ctx(),
        );
        assert_eq!(harness.recorder.take(), []);
    }

    #[test]
    fn nothing_is_reported_while_disconnected() {
        let (mut harness, mut plugin) = connected();
        harness.disconnect();
        plugin.disconnected(&mut harness.ctx());
        let now = now_millis();
        plugin.packet(
            &push(now, vec![sms(95, 1, SENDER, "Hello", now)]),
            &mut harness.ctx(),
        );
        assert_eq!(harness.recorder.take(), []);
    }

    #[tokio::test]
    async fn asks_again_until_answered_and_stops_when_disconnected() {
        let mut harness = Harness::new();
        let mut plugin = Sms::new(Timing {
            retry: Duration::from_millis(10),
            refresh: Duration::from_millis(40),
        });
        plugin.connected(&mut harness.ctx());

        for _ in 0..3 {
            let packet = harness.next_sent().await;
            assert_eq!(packet.kind, TYPE_SMS_REQUEST_CONVERSATION);
        }

        plugin.disconnected(&mut harness.ctx());
        assert!(plugin.requester.is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        harness.sent();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(harness.sent(), []);
    }
}
