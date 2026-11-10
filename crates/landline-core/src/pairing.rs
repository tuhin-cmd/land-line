//! Pairing with one peer, as a sans-I/O state machine.
//!
//! [`Pairing`] owns no socket, timer, clock or storage. Each input method
//! returns the [`Action`]s the caller must carry out, in order: write a
//! packet, start or cancel the single pairing timer, or report an event.
//! Time is passed in as Unix seconds, so behaviour is deterministic.
//!
//! The rules follow the KDE Connect apps (protocol 8):
//!
//! - A pair request is `{"pair": true, "timestamp": <unix seconds>}`; an
//!   accept is `{"pair": true}`; a rejection, cancellation or unpair is
//!   `{"pair": false}`.
//! - The requester gives up after 30 s; the side asked gives up after 25 s
//!   so its own timeout fires before the requester's.
//! - Both sides show a [`verification_key`] derived from the two public keys
//!   and the request's timestamp, so the user can spot a man in the middle.

use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::packet::{Packet, TYPE_PAIR};

/// How long we wait for the peer to answer our request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long we wait for the user to answer the peer's request. Shorter than
/// the peer's own 30 s so we never accept a request the peer abandoned.
pub const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
/// How far a peer's request timestamp may be from our clock. The timestamp
/// feeds the verification key, so a wildly wrong clock would show the user
/// keys that never match.
pub const MAX_CLOCK_DIFFERENCE_SECS: i64 = 1800;

/// Where pairing with one peer stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairState {
    NotPaired,
    /// We sent a request and are waiting for the peer.
    Requested,
    /// The peer sent a request and is waiting for the user.
    RequestedByPeer,
    Paired,
}

/// Names one start of the pairing timer, so a timer that fires after it was
/// cancelled or replaced can be recognised and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

/// Why a pairing attempt ended without pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum PairingFailure {
    #[error("timed out")]
    TimedOut,
    /// The peer's request timestamp is more than
    /// [`MAX_CLOCK_DIFFERENCE_SECS`] from our clock.
    #[error("the device clocks do not match")]
    ClocksDoNotMatch,
    #[error("cancelled by user")]
    CancelledByUser,
    #[error("cancelled by peer")]
    CancelledByPeer,
    /// The caller reported, through [`Pairing::send_failed`], that our
    /// request or accept could not be written.
    #[error("device not reachable")]
    NotReachable,
}

/// Something the caller reports to the user or acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingEvent {
    /// The peer asked to pair. Show [`Pairing::verification_key`] and ask the
    /// user, then call [`Pairing::accept`] or [`Pairing::cancel`].
    IncomingRequest,
    /// Pairing completed: pin the peer's certificate and persist the trust.
    Paired,
    /// A pairing that existed is gone: forget the peer's certificate.
    Unpaired,
    /// A pairing attempt ended. Nothing was trusted, so nothing to forget.
    Failed(PairingFailure),
}

/// One thing the caller must do, in the order returned.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this packet to the peer. If the write fails, call
    /// [`Pairing::send_failed`].
    Send(Packet),
    /// Start (or restart) the one pairing timer; call
    /// [`Pairing::timed_out`] with `timer` when it fires. Starting replaces
    /// any timer already running.
    StartTimer { timer: TimerId, after: Duration },
    /// Stop the running pairing timer. Harmless if it already fired.
    CancelTimer,
    /// Report this to the user or act on it.
    Event(PairingEvent),
}

/// The pairing state machine for one peer device.
#[derive(Debug, Clone)]
pub struct Pairing {
    state: PairState,
    peer_protocol_version: i64,
    /// The timestamp of the pending request, ours or the peer's. `None` when
    /// nothing is pending, or when a pre-8 peer sent no timestamp.
    timestamp: Option<i64>,
    timer: Option<TimerId>,
    timers_started: u64,
    /// We reached `Paired` by sending an accept, and the caller has not yet
    /// reported that write as failed. Only meaningful while `Paired`; every
    /// way back into `Paired` resets it.
    accept_unconfirmed: bool,
}

impl Pairing {
    /// A machine for a peer speaking `peer_protocol_version`, starting
    /// `Paired` if the caller already trusts it and `NotPaired` otherwise.
    pub fn new(peer_protocol_version: i64, paired: bool) -> Self {
        Self {
            state: if paired {
                PairState::Paired
            } else {
                PairState::NotPaired
            },
            peer_protocol_version,
            timestamp: None,
            timer: None,
            timers_started: 0,
            accept_unconfirmed: false,
        }
    }

    pub fn state(&self) -> PairState {
        self.state
    }

    pub fn is_paired(&self) -> bool {
        self.state == PairState::Paired
    }

    pub fn peer_protocol_version(&self) -> i64 {
        self.peer_protocol_version
    }

    /// The Unix-seconds timestamp of the pending request, ours or the
    /// peer's.
    pub fn pending_timestamp(&self) -> Option<i64> {
        match self.state {
            PairState::Requested | PairState::RequestedByPeer => self.timestamp,
            PairState::NotPaired | PairState::Paired => None,
        }
    }

    /// The key to show the user while a request is pending in either
    /// direction; `None` otherwise. `our_public_key` and `peer_public_key`
    /// are the DER `SubjectPublicKeyInfo` of each side's certificate.
    pub fn verification_key(
        &self,
        our_public_key: &[u8],
        peer_public_key: &[u8],
    ) -> Option<String> {
        self.pending_timestamp()
            .map(|timestamp| verification_key(our_public_key, peer_public_key, timestamp))
    }

    /// The user asked to pair. Does nothing if already paired, and accepts
    /// if the peer has already asked. Asking again while our request is
    /// pending sends a fresh request with a new timestamp.
    pub fn request(&mut self, now_secs: i64) -> Vec<Action> {
        match self.state {
            PairState::Paired => Vec::new(),
            PairState::RequestedByPeer => self.accept(),
            PairState::NotPaired | PairState::Requested => {
                let mut actions = self.cancel_timer();
                self.state = PairState::Requested;
                self.timestamp = Some(now_secs);
                actions.push(self.start_timer(REQUEST_TIMEOUT));
                actions.push(Action::Send(pair_packet(true, Some(now_secs))));
                actions
            }
        }
    }

    /// The user accepted the peer's request. Does nothing unless the peer
    /// asked.
    pub fn accept(&mut self) -> Vec<Action> {
        if self.state != PairState::RequestedByPeer {
            return Vec::new();
        }
        let mut actions = self.cancel_timer();
        actions.push(Action::Send(pair_packet(true, None)));
        actions.push(self.become_paired());
        self.accept_unconfirmed = true;
        actions
    }

    /// The user rejected the peer's request or cancelled ours. Does nothing
    /// unless a request is pending.
    pub fn cancel(&mut self) -> Vec<Action> {
        if !self.is_pending() {
            return Vec::new();
        }
        let mut actions = self.cancel_timer();
        actions.push(Action::Send(unpair_packet()));
        actions.push(self.fail(PairingFailure::CancelledByUser));
        actions
    }

    /// The user asked to unpair. Tells the peer only if `reachable`, and
    /// reports [`PairingEvent::Unpaired`] only if there was something to
    /// undo. Sending even when already unpaired lets a peer that still
    /// thinks it is paired catch up.
    pub fn unpair(&mut self, reachable: bool) -> Vec<Action> {
        let mut actions = self.cancel_timer();
        if reachable {
            actions.push(Action::Send(unpair_packet()));
        }
        if self.state != PairState::NotPaired {
            self.state = PairState::NotPaired;
            self.timestamp = None;
            actions.push(Action::Event(PairingEvent::Unpaired));
        }
        actions
    }

    /// A packet arrived from the peer. Packets of other types are ignored,
    /// so the caller may pass everything through.
    pub fn packet_received(&mut self, packet: &Packet, now_secs: i64) -> Vec<Action> {
        if packet.kind != TYPE_PAIR {
            return Vec::new();
        }
        // Both KDE Connect apps read a missing or malformed `pair` as false.
        if packet.body_bool("pair").unwrap_or(false) {
            self.pair_received(packet, now_secs)
        } else {
            self.unpair_received()
        }
    }

    /// The timer started with `timer` fired. Ignored if that timer was
    /// cancelled or replaced since.
    pub fn timed_out(&mut self, timer: TimerId) -> Vec<Action> {
        if self.timer != Some(timer) {
            return Vec::new();
        }
        self.timer = None;
        if !self.is_pending() {
            return Vec::new();
        }
        // The Android app sends nothing here and lets the peer time out on
        // its own; the desktop app tells it, which clears a stale prompt on
        // the phone sooner.
        vec![
            Action::Send(unpair_packet()),
            self.fail(PairingFailure::TimedOut),
        ]
    }

    /// The caller could not write the packet from the most recent
    /// [`Action::Send`]. Undoes a request, or an accept that had already
    /// reported [`PairingEvent::Paired`] (reporting
    /// [`PairingEvent::Unpaired`] first, so the caller forgets the
    /// certificate). Ignored after anything else.
    pub fn send_failed(&mut self) -> Vec<Action> {
        let undo_accept = std::mem::take(&mut self.accept_unconfirmed);
        match self.state {
            PairState::Requested => {
                let mut actions = self.cancel_timer();
                actions.push(self.fail(PairingFailure::NotReachable));
                actions
            }
            PairState::Paired if undo_accept => vec![
                Action::Event(PairingEvent::Unpaired),
                self.fail(PairingFailure::NotReachable),
            ],
            _ => Vec::new(),
        }
    }

    fn pair_received(&mut self, packet: &Packet, now_secs: i64) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.state {
            // The peer accepted ours. Accepts carry no timestamp, and a
            // request that crossed ours on the wire counts as an accept too.
            PairState::Requested => {
                actions.extend(self.cancel_timer());
                actions.push(self.become_paired());
                return actions;
            }
            PairState::RequestedByPeer => return actions,
            // A request and an accept look alike before protocol 8, so
            // accepting automatically could loop between two peers; the
            // Android app unpairs and treats it as a new request instead.
            PairState::Paired => {
                self.state = PairState::NotPaired;
                actions.push(Action::Event(PairingEvent::Unpaired));
            }
            PairState::NotPaired => {}
        }

        let timestamp = packet.body_i64("timestamp");
        if self.peer_protocol_version >= 8 {
            let Some(timestamp) = timestamp else {
                return actions;
            };
            if timestamp.abs_diff(now_secs) > MAX_CLOCK_DIFFERENCE_SECS.unsigned_abs() {
                actions.push(self.fail(PairingFailure::ClocksDoNotMatch));
                return actions;
            }
        }

        self.state = PairState::RequestedByPeer;
        self.timestamp = timestamp;
        actions.push(self.start_timer(PEER_REQUEST_TIMEOUT));
        actions.push(Action::Event(PairingEvent::IncomingRequest));
        actions
    }

    fn unpair_received(&mut self) -> Vec<Action> {
        let mut actions = self.cancel_timer();
        match self.state {
            PairState::NotPaired => {}
            PairState::Requested | PairState::RequestedByPeer => {
                actions.push(self.fail(PairingFailure::CancelledByPeer));
            }
            PairState::Paired => {
                self.state = PairState::NotPaired;
                actions.push(Action::Event(PairingEvent::Unpaired));
            }
        }
        actions
    }

    fn is_pending(&self) -> bool {
        matches!(
            self.state,
            PairState::Requested | PairState::RequestedByPeer
        )
    }

    fn become_paired(&mut self) -> Action {
        self.state = PairState::Paired;
        self.timestamp = None;
        self.accept_unconfirmed = false;
        Action::Event(PairingEvent::Paired)
    }

    fn fail(&mut self, failure: PairingFailure) -> Action {
        self.state = PairState::NotPaired;
        self.timestamp = None;
        Action::Event(PairingEvent::Failed(failure))
    }

    fn start_timer(&mut self, after: Duration) -> Action {
        self.timers_started += 1;
        let timer = TimerId(self.timers_started);
        self.timer = Some(timer);
        Action::StartTimer { timer, after }
    }

    fn cancel_timer(&mut self) -> Vec<Action> {
        match self.timer.take() {
            Some(_) => vec![Action::CancelTimer],
            None => Vec::new(),
        }
    }
}

/// `{"pair": false}`: rejects, cancels or ends a pairing. The link layer
/// also sends it on its own when an unpaired peer sends any other packet,
/// so the peer learns it is not trusted.
pub fn unpair_packet() -> Packet {
    pair_packet(false, None)
}

fn pair_packet(pair: bool, timestamp: Option<i64>) -> Packet {
    let mut body = Map::new();
    body.insert("pair".into(), Value::Bool(pair));
    if let Some(timestamp) = timestamp {
        body.insert("timestamp".into(), timestamp.into());
    }
    Packet::new(TYPE_PAIR, body)
}

/// The protocol 8 verification key: eight uppercase hex characters that both
/// devices show during pairing.
///
/// The two DER `SubjectPublicKeyInfo` encodings are concatenated larger
/// first (unsigned bytewise; a strict prefix is smaller), so both sides get
/// the same bytes regardless of which is "ours". The request's timestamp is
/// appended in decimal ASCII, the whole is hashed with SHA-256, and the first
/// four bytes of the hash are shown.
pub fn verification_key(public_key_a: &[u8], public_key_b: &[u8], timestamp_secs: i64) -> String {
    let (larger, smaller) = if public_key_a >= public_key_b {
        (public_key_a, public_key_b)
    } else {
        (public_key_b, public_key_a)
    };
    let mut hasher = Sha256::new();
    hasher.update(larger);
    hasher.update(smaller);
    hasher.update(timestamp_secs.to_string().as_bytes());
    hasher.finalize()[..4]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_737_228_658;

    fn pair(body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("test body must be an object")
        };
        Packet::new(TYPE_PAIR, body)
    }

    fn peer_request(timestamp: i64) -> Packet {
        pair(json!({"pair": true, "timestamp": timestamp}))
    }

    fn peer_accept() -> Packet {
        pair(json!({"pair": true}))
    }

    fn peer_unpair() -> Packet {
        pair(json!({"pair": false}))
    }

    fn sent(actions: &[Action]) -> Vec<Value> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Send(packet) => {
                    assert_eq!(packet.kind, TYPE_PAIR);
                    Some(Value::Object(packet.body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn events(actions: &[Action]) -> Vec<PairingEvent> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Event(event) => Some(*event),
                _ => None,
            })
            .collect()
    }

    fn started_timer(actions: &[Action]) -> Option<(TimerId, Duration)> {
        actions.iter().find_map(|action| match action {
            Action::StartTimer { timer, after } => Some((*timer, *after)),
            _ => None,
        })
    }

    fn cancels_timer(actions: &[Action]) -> bool {
        actions.contains(&Action::CancelTimer)
    }

    fn requested() -> (Pairing, TimerId) {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.request(NOW);
        (pairing, started_timer(&actions).unwrap().0)
    }

    fn requested_by_peer() -> (Pairing, TimerId) {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.packet_received(&peer_request(NOW), NOW);
        (pairing, started_timer(&actions).unwrap().0)
    }

    // Public keys from the test certificates in the KDE Connect Android
    // app's PairingHandlerTest (generated test keys, not a real device's).
    const KEY_A: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004aa3882939b98fac7c0ca8184a422d2e68c28085cfd2090e5ba6e656f9680c6b189efa982cc1467bb616f20f254ffa8f05311f0ce33852943c5fa8e2744f97ae7";
    const KEY_B: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004395b9debac3269b3dbffb2ccd07f478bb6fa068916dfb18fac9cee882497e2d6915e1c3423da7004f9a4f25187a5adcc75788baac451e725316dfc7a97c2e02f";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn verification_key_matches_the_android_test_vectors() {
        let (a, b) = (hex(KEY_A), hex(KEY_B));
        assert_eq!(verification_key(&a, &b, 1_737_228_658), "54DC916E");
        assert_eq!(verification_key(&b, &a, 1_737_228_658), "54DC916E");
        assert_eq!(verification_key(&a, &b, 2_737_228_658), "8C07153A");
    }

    #[test]
    fn verification_key_puts_the_larger_key_first() {
        let digest = |bytes: &[u8]| -> String {
            Sha256::digest(bytes)[..4]
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect()
        };
        // 0x80 sorts above 0x7f only when compared unsigned.
        assert_eq!(verification_key(&[0x7f], &[0x80], 5), digest(b"\x80\x7f5"));
        // A strict prefix is the smaller of the two.
        assert_eq!(verification_key(&[1], &[1, 0], 5), digest(b"\x01\x00\x015"));
        assert_eq!(
            verification_key(&[1, 0], &[1], -3),
            digest(b"\x01\x00\x01-3")
        );
    }

    #[test]
    fn starts_in_the_given_state() {
        assert_eq!(Pairing::new(8, false).state(), PairState::NotPaired);
        assert!(Pairing::new(8, true).is_paired());
        assert_eq!(Pairing::new(7, false).peer_protocol_version(), 7);
    }

    #[test]
    fn request_sends_a_timestamped_request_and_starts_a_30s_timer() {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.request(NOW);
        assert_eq!(pairing.state(), PairState::Requested);
        assert_eq!(sent(&actions), [json!({"pair": true, "timestamp": NOW})]);
        assert_eq!(started_timer(&actions).unwrap().1, REQUEST_TIMEOUT);
        assert!(events(&actions).is_empty());
        assert_eq!(pairing.pending_timestamp(), Some(NOW));
    }

    #[test]
    fn requesting_again_sends_a_fresh_request_and_restarts_the_timer() {
        let (mut pairing, first) = requested();
        let actions = pairing.request(NOW + 5);
        assert!(cancels_timer(&actions));
        let (second, _) = started_timer(&actions).unwrap();
        assert_ne!(first, second);
        assert_eq!(sent(&actions)[0]["timestamp"], json!(NOW + 5));
        assert_eq!(pairing.pending_timestamp(), Some(NOW + 5));
        // The replaced timer firing late changes nothing.
        assert!(pairing.timed_out(first).is_empty());
        assert_eq!(pairing.state(), PairState::Requested);
    }

    #[test]
    fn request_while_paired_does_nothing() {
        let mut pairing = Pairing::new(8, true);
        assert!(pairing.request(NOW).is_empty());
        assert!(pairing.is_paired());
    }

    #[test]
    fn peer_accept_completes_our_request() {
        let (mut pairing, _) = requested();
        let actions = pairing.packet_received(&peer_accept(), NOW + 3);
        assert_eq!(pairing.state(), PairState::Paired);
        assert!(cancels_timer(&actions));
        assert!(sent(&actions).is_empty());
        assert_eq!(events(&actions), [PairingEvent::Paired]);
        assert_eq!(pairing.pending_timestamp(), None);
    }

    #[test]
    fn crossing_requests_pair_without_a_clock_check() {
        let (mut pairing, _) = requested();
        let actions = pairing.packet_received(&peer_request(NOW - 99_999), NOW);
        assert_eq!(events(&actions), [PairingEvent::Paired]);
    }

    #[test]
    fn our_request_times_out() {
        let (mut pairing, timer) = requested();
        let actions = pairing.timed_out(timer);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(sent(&actions), [json!({"pair": false})]);
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::TimedOut)]
        );
        // A second firing of the same timer is stale.
        assert!(pairing.timed_out(timer).is_empty());
    }

    #[test]
    fn peer_rejects_our_request() {
        let (mut pairing, _) = requested();
        let actions = pairing.packet_received(&peer_unpair(), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(cancels_timer(&actions));
        assert!(sent(&actions).is_empty());
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::CancelledByPeer)]
        );
    }

    #[test]
    fn we_cancel_our_request() {
        let (mut pairing, _) = requested();
        let actions = pairing.cancel();
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(cancels_timer(&actions));
        assert_eq!(sent(&actions), [json!({"pair": false})]);
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::CancelledByUser)]
        );
    }

    #[test]
    fn failing_to_send_our_request_ends_it() {
        let (mut pairing, _) = requested();
        let actions = pairing.send_failed();
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(cancels_timer(&actions));
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::NotReachable)]
        );
    }

    #[test]
    fn peer_request_waits_for_the_user_for_25s() {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.packet_received(&peer_request(NOW + 60), NOW);
        assert_eq!(pairing.state(), PairState::RequestedByPeer);
        assert!(sent(&actions).is_empty());
        assert_eq!(started_timer(&actions).unwrap().1, PEER_REQUEST_TIMEOUT);
        assert_eq!(events(&actions), [PairingEvent::IncomingRequest]);
        assert_eq!(pairing.pending_timestamp(), Some(NOW + 60));
    }

    #[test]
    fn peer_request_accepts_a_string_timestamp() {
        let mut pairing = Pairing::new(8, false);
        let packet = pair(json!({"pair": "true", "timestamp": NOW.to_string()}));
        pairing.packet_received(&packet, NOW);
        assert_eq!(pairing.pending_timestamp(), Some(NOW));
    }

    #[test]
    fn peer_request_needs_a_timestamp_from_protocol_8() {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.packet_received(&peer_accept(), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(actions.is_empty());
    }

    #[test]
    fn older_peers_may_omit_the_timestamp() {
        let mut pairing = Pairing::new(7, false);
        let actions = pairing.packet_received(&peer_accept(), NOW);
        assert_eq!(pairing.state(), PairState::RequestedByPeer);
        assert_eq!(events(&actions), [PairingEvent::IncomingRequest]);
        // The protocol 8 key needs a timestamp.
        assert_eq!(pairing.verification_key(&[1], &[2]), None);
        // Nor is their clock checked.
        let mut pairing = Pairing::new(7, false);
        pairing.packet_received(&peer_request(0), NOW);
        assert_eq!(pairing.state(), PairState::RequestedByPeer);
    }

    #[test]
    fn peer_request_clock_window_is_1800s_either_way() {
        for (offset, accepted) in [(1800, true), (-1800, true), (1801, false), (-1801, false)] {
            let mut pairing = Pairing::new(8, false);
            let actions = pairing.packet_received(&peer_request(NOW + offset), NOW);
            if accepted {
                assert_eq!(pairing.state(), PairState::RequestedByPeer, "{offset}");
            } else {
                assert_eq!(pairing.state(), PairState::NotPaired, "{offset}");
                assert_eq!(
                    events(&actions),
                    [PairingEvent::Failed(PairingFailure::ClocksDoNotMatch)]
                );
                assert!(started_timer(&actions).is_none());
            }
        }
    }

    #[test]
    fn extreme_timestamps_do_not_overflow() {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.packet_received(&peer_request(i64::MIN), i64::MAX);
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::ClocksDoNotMatch)]
        );
    }

    #[test]
    fn second_peer_request_is_ignored() {
        let (mut pairing, timer) = requested_by_peer();
        assert!(
            pairing
                .packet_received(&peer_request(NOW + 9), NOW)
                .is_empty()
        );
        assert_eq!(pairing.state(), PairState::RequestedByPeer);
        assert_eq!(pairing.pending_timestamp(), Some(NOW));
        // The original timer still counts.
        assert!(!pairing.timed_out(timer).is_empty());
    }

    #[test]
    fn accept_sends_an_untimestamped_accept() {
        let (mut pairing, _) = requested_by_peer();
        let actions = pairing.accept();
        assert_eq!(pairing.state(), PairState::Paired);
        assert!(cancels_timer(&actions));
        assert_eq!(sent(&actions), [json!({"pair": true})]);
        assert_eq!(events(&actions), [PairingEvent::Paired]);
    }

    #[test]
    fn request_while_requested_by_peer_accepts() {
        let (mut pairing, _) = requested_by_peer();
        let actions = pairing.request(NOW);
        assert_eq!(pairing.state(), PairState::Paired);
        assert_eq!(sent(&actions), [json!({"pair": true})]);
        assert_eq!(events(&actions), [PairingEvent::Paired]);
    }

    #[test]
    fn accept_needs_a_peer_request() {
        for paired in [false, true] {
            let mut pairing = Pairing::new(8, paired);
            assert!(pairing.accept().is_empty());
            assert_eq!(pairing.is_paired(), paired);
        }
        let (mut pairing, _) = requested();
        assert!(pairing.accept().is_empty());
        assert_eq!(pairing.state(), PairState::Requested);
    }

    #[test]
    fn failing_to_send_an_accept_undoes_it() {
        let (mut pairing, _) = requested_by_peer();
        pairing.accept();
        let actions = pairing.send_failed();
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(
            events(&actions),
            [
                PairingEvent::Unpaired,
                PairingEvent::Failed(PairingFailure::NotReachable)
            ]
        );
    }

    #[test]
    fn send_failures_after_other_actions_are_ignored() {
        let mut paired = Pairing::new(8, true);
        assert!(paired.send_failed().is_empty());
        assert!(paired.is_paired());

        // Paired by the peer's accept: we sent nothing that could fail.
        let (mut pairing, _) = requested();
        pairing.packet_received(&peer_accept(), NOW);
        assert!(pairing.send_failed().is_empty());
        assert!(pairing.is_paired());

        // A failure is reported once.
        let (mut pairing, _) = requested_by_peer();
        pairing.accept();
        assert!(!pairing.send_failed().is_empty());
        assert!(pairing.send_failed().is_empty());

        let (mut pairing, _) = requested_by_peer();
        pairing.cancel();
        assert!(pairing.send_failed().is_empty());
    }

    #[test]
    fn we_reject_a_peer_request() {
        let (mut pairing, _) = requested_by_peer();
        let actions = pairing.cancel();
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(cancels_timer(&actions));
        assert_eq!(sent(&actions), [json!({"pair": false})]);
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::CancelledByUser)]
        );
    }

    #[test]
    fn cancel_needs_a_pending_request() {
        for paired in [false, true] {
            let mut pairing = Pairing::new(8, paired);
            assert!(pairing.cancel().is_empty());
            assert_eq!(pairing.is_paired(), paired);
        }
    }

    #[test]
    fn peer_withdraws_its_request() {
        let (mut pairing, _) = requested_by_peer();
        let actions = pairing.packet_received(&peer_unpair(), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(cancels_timer(&actions));
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::CancelledByPeer)]
        );
    }

    #[test]
    fn peer_request_times_out() {
        let (mut pairing, timer) = requested_by_peer();
        let actions = pairing.timed_out(timer);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(
            events(&actions),
            [PairingEvent::Failed(PairingFailure::TimedOut)]
        );
    }

    #[test]
    fn peer_unpairs() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.packet_received(&peer_unpair(), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert!(sent(&actions).is_empty());
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);
    }

    #[test]
    fn missing_pair_flag_reads_as_unpair() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.packet_received(&pair(json!({"pair": "maybe"})), NOW);
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);
    }

    #[test]
    fn unpair_when_not_paired_is_ignored() {
        let mut pairing = Pairing::new(8, false);
        assert!(pairing.packet_received(&peer_unpair(), NOW).is_empty());
        assert_eq!(pairing.state(), PairState::NotPaired);
    }

    #[test]
    fn request_while_paired_unpairs_then_starts_over() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.packet_received(&peer_request(NOW), NOW);
        assert_eq!(pairing.state(), PairState::RequestedByPeer);
        assert_eq!(
            events(&actions),
            [PairingEvent::Unpaired, PairingEvent::IncomingRequest]
        );
        assert!(started_timer(&actions).is_some());
    }

    #[test]
    fn request_while_paired_without_timestamp_just_unpairs() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.packet_received(&peer_accept(), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);
    }

    #[test]
    fn request_while_paired_with_bad_clock_unpairs_and_fails() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.packet_received(&peer_request(NOW + 5000), NOW);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(
            events(&actions),
            [
                PairingEvent::Unpaired,
                PairingEvent::Failed(PairingFailure::ClocksDoNotMatch)
            ]
        );
    }

    #[test]
    fn we_unpair() {
        let mut pairing = Pairing::new(8, true);
        let actions = pairing.unpair(true);
        assert_eq!(pairing.state(), PairState::NotPaired);
        assert_eq!(sent(&actions), [json!({"pair": false})]);
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);

        let mut pairing = Pairing::new(8, true);
        let actions = pairing.unpair(false);
        assert!(sent(&actions).is_empty());
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);
    }

    #[test]
    fn unpair_during_a_request_stops_the_timer() {
        let (mut pairing, timer) = requested();
        let actions = pairing.unpair(true);
        assert!(cancels_timer(&actions));
        assert_eq!(events(&actions), [PairingEvent::Unpaired]);
        assert!(pairing.timed_out(timer).is_empty());
    }

    #[test]
    fn unpair_when_not_paired_still_tells_the_peer() {
        let mut pairing = Pairing::new(8, false);
        let actions = pairing.unpair(true);
        assert_eq!(sent(&actions).len(), 1);
        assert!(events(&actions).is_empty());
    }

    #[test]
    fn timer_after_completion_is_ignored() {
        let (mut pairing, timer) = requested();
        pairing.packet_received(&peer_accept(), NOW);
        assert!(pairing.timed_out(timer).is_empty());
        assert!(pairing.is_paired());
    }

    #[test]
    fn non_pair_packets_are_ignored() {
        let mut pairing = Pairing::new(8, true);
        let ping = Packet::new("kdeconnect.ping", Map::new());
        assert!(pairing.packet_received(&ping, NOW).is_empty());
        assert!(pairing.is_paired());
    }

    #[test]
    fn verification_key_only_while_pending() {
        let (a, b) = (hex(KEY_A), hex(KEY_B));
        let mut pairing = Pairing::new(8, false);
        assert_eq!(pairing.verification_key(&a, &b), None);

        // Our request: our timestamp.
        pairing.request(NOW);
        assert_eq!(
            pairing.verification_key(&a, &b).as_deref(),
            Some("54DC916E")
        );
        pairing.packet_received(&peer_accept(), NOW);
        assert_eq!(pairing.verification_key(&a, &b), None);

        // Their request: their timestamp, whatever our clock says.
        let mut pairing = Pairing::new(8, false);
        pairing.packet_received(&peer_request(NOW), NOW + 100);
        assert_eq!(
            pairing.verification_key(&b, &a).as_deref(),
            Some("54DC916E")
        );
    }

    #[test]
    fn unpair_packet_shape() {
        let packet = unpair_packet();
        assert_eq!(packet.kind, TYPE_PAIR);
        assert_eq!(Value::Object(packet.body), json!({"pair": false}));
    }
}
