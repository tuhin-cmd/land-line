//! Notifications: the phone's notifications mirrored to the Mac.
//!
//! What the Android app (`plugins/notifications/NotificationsPlugin.kt`)
//! sends and expects:
//!
//! - `kdeconnect.notification`, phone to us, for a posted or updated
//!   notification: `id` (the Android notification key; the same id replaces),
//!   `appName`, `time` (ms since the epoch, as a string), `isClearable`,
//!   `silent`, and, unless the user hid the app's contents, `ticker`
//!   ("title: text") with `title` and `text` each present only if the app
//!   set them. `payloadHash` names the icon (MD5 of the PNG, lowercase hex);
//!   the PNG itself is attached as a payload only the first time that icon
//!   goes out for that id on this connection. `actions`, `requestReplyId`,
//!   `conversation` and `groupName` are ignored here.
//! - `kdeconnect.notification`, phone to us, for a removal: just `id` and
//!   `isCancel: true`.
//! - `kdeconnect.notification.request`, us to phone: `{"request": true}`
//!   makes it resend every current notification with `silent: true`;
//!   `{"cancel": id}` dismisses one, after which the phone sends the removal.
//!
//! The phone also sends its current notifications, silently, on its own
//! when its plugin starts for us, so the request on connect mostly yields
//! duplicates, which are harmless updates.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Map;
use tokio::io::AsyncReadExt;
use tracing::{debug, trace};

use super::{Command, Context, Plugin, Registration};
use crate::CoreDelegate;
use crate::packet::Packet;
use crate::payload::Incoming;

pub(crate) const TYPE_NOTIFICATION: &str = "kdeconnect.notification";
pub(crate) const TYPE_NOTIFICATION_REQUEST: &str = "kdeconnect.notification.request";

/// The phone sends 96 px app icons and contact photos of similar size, a
/// few kilobytes as PNG. Anything this large is not an icon.
const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;

/// Bounds a whole icon download. The payload connection has its own connect
/// and handshake timeouts but none for a peer that stalls mid-transfer.
const ICON_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A notification the phone marks silent (it was already showing, or the
/// user only wants alerts while the phone is locked) is still scanned for a
/// one-time code if it is this recent. Older codes have expired, and
/// scanning them would copy a stale code on every reconnect.
const OTP_MAX_AGE: Duration = Duration::from_secs(10 * 60);

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// A notification showing on the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneNotification {
    /// Identifies the notification on its device. A later notification with
    /// the same ID replaces this one.
    pub id: String,
    pub app_name: String,
    /// Never empty together with `text`: a notification without either
    /// (the user hid that app's contents on the phone) has the app name as
    /// its title.
    pub title: String,
    pub text: String,
    /// When the phone posted it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// Whether [`crate::Core::dismiss_notification`] can remove it.
    pub dismissable: bool,
    /// Update the notification without announcing it. Set when it was
    /// already showing on the phone when we asked for the current list, when
    /// the phone asks for it, and when the core posts a notification again
    /// only to add its icon.
    pub silent: bool,
    /// The app's icon as a PNG file in the core's cache, if the phone sent
    /// one. The cache keeps the file; copy it before handing it to an API
    /// that moves its input.
    pub icon_path: Option<PathBuf>,
}

pub(crate) const REGISTRATION: Registration = Registration {
    name: "notifications",
    incoming: &[TYPE_NOTIFICATION],
    outgoing: &[TYPE_NOTIFICATION_REQUEST],
    create: || Box::<Notifications>::default(),
};

/// Shared with the icon download tasks, which post a notification again
/// once its icon is saved. Every post and removal happens under this lock,
/// so a download that finishes after a removal cannot bring the
/// notification back.
#[derive(Default)]
struct Notifications {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// The notifications posted to the delegate and not yet removed.
    live: HashMap<String, Live>,
    /// Icon hashes being downloaded, so a burst of notifications from one app
    /// (the phone attaches the icon to each) fetches it once.
    downloading: HashSet<String>,
}

struct Live {
    /// As last posted.
    notification: PhoneNotification,
    icon_hash: Option<String>,
    /// What was last scanned for a one-time code.
    otp_text: String,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Plugin for Notifications {
    fn connected(&mut self, ctx: &mut Context<'_>) {
        let mut body = Map::new();
        body.insert("request".into(), true.into());
        ctx.send(Packet::new(TYPE_NOTIFICATION_REQUEST, body));
    }

    fn link_replaced(&mut self, ctx: &mut Context<'_>) {
        // The notifications are still showing on the phone. Starting over
        // would take every banner off the Mac and list them again as old
        // ones, every few minutes. Asking again is enough to catch up.
        self.connected(ctx);
    }

    fn disconnected(&mut self, ctx: &mut Context<'_>) {
        // The phone will send what is still showing when it is back; until
        // then the Mac would show notifications that may be long gone.
        let mut state = lock(&self.state);
        for (id, _) in state.live.drain() {
            ctx.delegate.notification_removed(ctx.device_id, &id);
        }
    }

    fn packet(&mut self, packet: &Packet, ctx: &mut Context<'_>) {
        let Some(id) = packet.body_str("id").filter(|id| !id.is_empty()) else {
            debug!("ignoring a notification without an id");
            return;
        };
        if packet.body_bool("isCancel") == Some(true) {
            self.remove(&id, ctx);
        } else {
            self.post(id, packet, ctx);
        }
    }

    fn command(&mut self, command: Command, ctx: &mut Context<'_>) {
        let Command::DismissNotification { id } = command else {
            return;
        };
        let mut state = lock(&self.state);
        if !state
            .live
            .get(&id)
            .is_some_and(|live| live.notification.dismissable)
        {
            debug!("not dismissing a notification that is unknown or not dismissable");
            return;
        }
        let mut body = Map::new();
        body.insert("cancel".into(), id.clone().into());
        if ctx.send(Packet::new(TYPE_NOTIFICATION_REQUEST, body)) {
            // Removed now rather than when the phone confirms: if the phone
            // no longer has it, no confirmation comes.
            state.live.remove(&id);
            ctx.delegate.notification_removed(ctx.device_id, &id);
        }
    }
}

impl Notifications {
    fn remove(&self, id: &str, ctx: &Context<'_>) {
        let mut state = lock(&self.state);
        if state.live.remove(id).is_some() {
            ctx.delegate.notification_removed(ctx.device_id, id);
        } else {
            trace!(id, "removal of a notification that was never posted");
        }
    }

    /// Posts at once, with the icon if it is cached. Otherwise the icon is
    /// fetched in the background and the notification posted again, silently,
    /// once it is saved. Waiting for the icon instead would delay the banner
    /// (which may carry a one-time code) on every notification whose icon is
    /// new, and a slow or failed download would delay or lose it; posting
    /// twice happens only the first time each icon is seen.
    fn post(&self, id: String, packet: &Packet, ctx: &mut Context<'_>) {
        let field = |key| packet.body_str(key).unwrap_or_default();
        let app_name = field("appName");
        let mut title = field("title");
        let mut text = field("text");
        if title.is_empty() && text.is_empty() {
            text = field("ticker");
        }
        let otp_text = otp_text(&title, &text);
        if title.is_empty() && text.is_empty() {
            if app_name.is_empty() {
                // Nothing to show. If it replaces something, that is gone.
                debug!("ignoring a notification with no app name, title or text");
                self.remove(&id, ctx);
                return;
            }
            title = app_name.clone();
        }

        let icon_hash = packet
            .body_str("payloadHash")
            .and_then(|hash| icon_name(&hash));
        let icon_file = icon_hash
            .as_deref()
            .map(|hash| icon_file(ctx.cache_dir, hash));
        let cached = icon_file.as_ref().filter(|file| file.is_file()).cloned();

        let notification = PhoneNotification {
            id: id.clone(),
            app_name,
            title,
            text,
            // The packet id is when the phone sent it, close enough when the
            // posting time is missing.
            time_ms: packet.body_i64("time").unwrap_or(packet.id),
            dismissable: packet.body_bool("isClearable").unwrap_or(false),
            silent: packet.body_bool("silent").unwrap_or(false),
            icon_path: cached.clone(),
        };

        let mut state = lock(&self.state);
        let previous = state.live.get(&id).map(|live| live.otp_text.as_str());
        if wants_otp_scan(
            previous,
            &otp_text,
            notification.silent,
            notification.time_ms,
            now_ms(),
        ) {
            ctx.scan_for_otp(&notification.app_name, &otp_text);
        }
        trace!(id, "posting a notification");
        ctx.delegate
            .notification_posted(ctx.device_id, &notification);

        if cached.is_none()
            && let (Some(hash), Some(file)) = (&icon_hash, icon_file)
            && !state.downloading.contains(hash)
            && let Some(incoming) = ctx.payload(packet)
        {
            if incoming.size().is_some_and(|size| size > MAX_ICON_BYTES) {
                debug!(
                    size = incoming.size(),
                    "refusing an oversized notification icon"
                );
            } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                state.downloading.insert(hash.clone());
                runtime.spawn(fetch_icon(IconFetch {
                    incoming,
                    file,
                    hash: hash.clone(),
                    state: Arc::clone(&self.state),
                    delegate: Arc::clone(ctx.delegate),
                    device_id: ctx.device_id.to_owned(),
                }));
            }
        }

        state.live.insert(
            id,
            Live {
                notification,
                icon_hash,
                otp_text,
            },
        );
    }
}

/// The text scanned for a one-time code: title and text, since some apps put
/// the code in the title.
fn otp_text(title: &str, text: &str) -> String {
    [title, text]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether to scan a notification's text for a one-time code. Not when the
/// text has not changed since the last scan, so an update to an old
/// notification does not bring its code back; and not when the phone marks
/// it silent and it is old, as for the ones listed on every reconnect.
fn wants_otp_scan(
    previous: Option<&str>,
    text: &str,
    silent: bool,
    time_ms: i64,
    now_ms: i64,
) -> bool {
    if text.is_empty() || previous == Some(text) {
        return false;
    }
    let max_age = i64::try_from(OTP_MAX_AGE.as_millis()).unwrap_or(i64::MAX);
    !silent || now_ms.saturating_sub(time_ms) <= max_age
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The cache file name for an icon hash, which comes from the network: only
/// hex digits are accepted, so it can never name another path.
fn icon_name(hash: &str) -> Option<String> {
    let valid = (1..=128).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| hash.to_ascii_lowercase())
}

fn icon_file(cache_dir: &Path, hash: &str) -> PathBuf {
    cache_dir.join("icons").join(format!("{hash}.png"))
}

struct IconFetch {
    incoming: Incoming,
    file: PathBuf,
    hash: String,
    state: Arc<Mutex<State>>,
    delegate: Arc<dyn CoreDelegate>,
    device_id: String,
}

async fn fetch_icon(fetch: IconFetch) {
    let saved = match tokio::time::timeout(ICON_FETCH_TIMEOUT, download(fetch.incoming)).await {
        Ok(Ok(png)) => save(fetch.file.clone(), png).await,
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "icon download stalled",
        )),
    };

    let mut state = lock(&fetch.state);
    state.downloading.remove(&fetch.hash);
    if let Err(error) = saved {
        debug!(%error, "could not fetch a notification icon");
        return;
    }
    // Every notification waiting for this icon, including updates that
    // arrived without the payload while it downloaded.
    for live in state.live.values_mut() {
        if live.icon_hash.as_deref() == Some(&fetch.hash) && live.notification.icon_path.is_none() {
            live.notification.icon_path = Some(fetch.file.clone());
            live.notification.silent = true;
            fetch
                .delegate
                .notification_posted(&fetch.device_id, &live.notification);
        }
    }
}

async fn download(incoming: Incoming) -> io::Result<Vec<u8>> {
    let mut png = Vec::new();
    incoming
        .open()
        .await?
        .take(MAX_ICON_BYTES + 1)
        .read_to_end(&mut png)
        .await?;
    if png.len() as u64 > MAX_ICON_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "icon too large"));
    }
    if !png.starts_with(PNG_SIGNATURE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "icon is not a PNG",
        ));
    }
    Ok(png)
}

/// Writes the icon under a temporary name and renames it into place, so a
/// notification never points at a half-written file, even with two
/// downloads of the same icon racing from two devices.
async fn save(file: PathBuf, png: Vec<u8>) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let dir = file
            .parent()
            .ok_or_else(|| io::Error::other("icon path has no directory"))?;
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join(format!(".{}.{unique}.tmp", std::process::id()));
        let written =
            std::fs::write(&temporary, &png).and_then(|()| std::fs::rename(&temporary, &file));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::AsyncWriteExt;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::payload::Offer;
    use crate::plugins::testing::{Event, Harness};

    const HASH: &str = "0123456789abcdef0123456789abcdef";

    fn packet(body: Value) -> Packet {
        let Value::Object(body) = body else {
            panic!("test body must be an object");
        };
        Packet::new(TYPE_NOTIFICATION, body)
    }

    /// A posted notification as the Android app sends it.
    fn posted(id: &str) -> Packet {
        packet(json!({
            "id": id,
            "appName": "Example Chat",
            "title": "Sample Sender",
            "text": "Sample message",
            "ticker": "Sample Sender: Sample message",
            "time": "1700000000000",
            "isClearable": true,
            "silent": false,
        }))
    }

    fn cancel(id: &str) -> Packet {
        packet(json!({"id": id, "isCancel": true}))
    }

    #[test]
    fn a_replaced_link_keeps_the_notifications_and_asks_again() {
        let mut harness = Harness::new();
        let mut plugin = (REGISTRATION.create)();
        plugin.packet(&posted("one"), &mut harness.ctx());
        harness.recorder.take();
        harness.sent();

        plugin.link_replaced(&mut harness.ctx());
        assert!(harness.recorder.take().is_empty());
        let sent = harness.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, TYPE_NOTIFICATION_REQUEST);

        // The notification is still known, so it can still be removed.
        plugin.packet(&cancel("one"), &mut harness.ctx());
        assert_eq!(
            harness.recorder.take(),
            [Event::NotificationRemoved("one".into())]
        );
    }

    fn posted_events(events: &[Event]) -> Vec<&PhoneNotification> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::NotificationPosted(notification) => Some(notification),
                _ => None,
            })
            .collect()
    }

    fn png() -> Vec<u8> {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(b"not really an image, but it starts like one");
        png
    }

    /// Plays the phone: offers `data` with `packet` and serves it once.
    async fn offer(harness: &Harness, packet: &mut Packet, data: Vec<u8>) -> JoinHandle<()> {
        let offer = Offer::bind(harness.device_side_peer()).await.unwrap();
        offer.attach(packet, data.len() as u64);
        tokio::spawn(async move {
            // The plugin may decline to fetch; that is not a failure here.
            if let Ok(mut stream) = offer.accept().await {
                let _ = stream.write_all(&data).await;
                let _ = stream.shutdown().await;
            }
        })
    }

    async fn until_no_downloads(plugin: &Notifications) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !lock(&plugin.state).downloading.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the icon download did not finish");
    }

    #[test]
    fn registers_the_types_the_android_app_uses() {
        assert_eq!(REGISTRATION.incoming, [TYPE_NOTIFICATION]);
        assert_eq!(REGISTRATION.outgoing, [TYPE_NOTIFICATION_REQUEST]);
    }

    #[test]
    fn asks_for_current_notifications_on_connect() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.connected(&mut harness.ctx());
        let sent = harness.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, TYPE_NOTIFICATION_REQUEST);
        assert_eq!(
            Value::Object(sent[0].body.clone()),
            json!({"request": true})
        );
    }

    #[test]
    fn posts_a_notification() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(
            &posted("0|com.example.chat|1|null|10001"),
            &mut harness.ctx(),
        );
        assert_eq!(
            harness.recorder.take(),
            [Event::NotificationPosted(PhoneNotification {
                id: "0|com.example.chat|1|null|10001".into(),
                app_name: "Example Chat".into(),
                title: "Sample Sender".into(),
                text: "Sample message".into(),
                time_ms: 1_700_000_000_000,
                dismissable: true,
                silent: false,
                icon_path: None,
            })]
        );
    }

    #[test]
    fn passes_on_silence_and_defaults_missing_flags() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut quiet = posted("a");
        quiet.body.insert("silent".into(), true.into());
        plugin.packet(&quiet, &mut harness.ctx());

        let mut bare = posted("b");
        bare.body.remove("silent");
        bare.body.remove("isClearable");
        bare.body.remove("time");
        let packet_time = bare.id;
        plugin.packet(&bare, &mut harness.ctx());

        let events = harness.recorder.take();
        let posts = posted_events(&events);
        assert!(posts[0].silent);
        assert!(!posts[1].silent);
        assert!(!posts[1].dismissable);
        assert_eq!(posts[1].time_ms, packet_time);
    }

    #[test]
    fn updates_replace_and_cancels_remove_only_what_was_posted() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(&posted("a"), &mut harness.ctx());
        let mut update = posted("a");
        update.body.insert("text".into(), "Another message".into());
        plugin.packet(&update, &mut harness.ctx());
        plugin.packet(&cancel("a"), &mut harness.ctx());
        plugin.packet(&cancel("a"), &mut harness.ctx());
        plugin.packet(&cancel("never-posted"), &mut harness.ctx());

        let events = harness.recorder.take();
        assert_eq!(events.len(), 3);
        assert_eq!(posted_events(&events)[1].text, "Another message");
        assert_eq!(events[2], Event::NotificationRemoved("a".into()));
    }

    #[test]
    fn ignores_packets_without_an_id() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut packet = posted("");
        plugin.packet(&packet, &mut harness.ctx());
        packet.body.remove("id");
        plugin.packet(&packet, &mut harness.ctx());
        assert!(harness.recorder.take().is_empty());
    }

    #[test]
    fn fills_in_empty_notifications() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();

        // Title only, text only: kept as they are.
        plugin.packet(
            &packet(json!({"id": "t", "appName": "Example", "title": "Only a title"})),
            &mut harness.ctx(),
        );
        // No title or text but a ticker: the ticker is the text.
        plugin.packet(
            &packet(json!({"id": "k", "appName": "Example", "ticker": "Ticker words"})),
            &mut harness.ctx(),
        );
        // Contents hidden by the user on the phone: only the app name.
        plugin.packet(
            &packet(json!({"id": "h", "appName": "Example"})),
            &mut harness.ctx(),
        );
        // Nothing at all to show.
        plugin.packet(&packet(json!({"id": "n", "title": ""})), &mut harness.ctx());

        let events = harness.recorder.take();
        let posts = posted_events(&events);
        assert_eq!(posts.len(), 3);
        assert_eq!(
            (posts[0].title.as_str(), posts[0].text.as_str()),
            ("Only a title", "")
        );
        assert_eq!(
            (posts[1].title.as_str(), posts[1].text.as_str()),
            ("", "Ticker words")
        );
        assert_eq!(
            (posts[2].title.as_str(), posts[2].text.as_str()),
            ("Example", "")
        );
    }

    #[test]
    fn a_notification_emptied_by_an_update_is_removed() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(&posted("a"), &mut harness.ctx());
        plugin.packet(&packet(json!({"id": "a"})), &mut harness.ctx());
        let events = harness.recorder.take();
        assert_eq!(events.last(), Some(&Event::NotificationRemoved("a".into())));
        assert!(lock(&plugin.state).live.is_empty());
    }

    #[test]
    fn disconnecting_removes_everything_posted() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(&posted("a"), &mut harness.ctx());
        plugin.packet(&posted("b"), &mut harness.ctx());
        harness.recorder.take();

        harness.disconnect();
        plugin.disconnected(&mut harness.ctx());
        let mut removed = harness.recorder.take();
        removed.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            removed,
            [
                Event::NotificationRemoved("a".into()),
                Event::NotificationRemoved("b".into())
            ]
        );

        // A late cancel for one of them reports nothing.
        plugin.packet(&cancel("a"), &mut harness.ctx());
        assert!(harness.recorder.take().is_empty());
    }

    #[test]
    fn dismisses_only_known_dismissable_notifications() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(&posted("clearable"), &mut harness.ctx());
        let mut pinned = posted("pinned");
        pinned.body.insert("isClearable".into(), false.into());
        plugin.packet(&pinned, &mut harness.ctx());
        harness.recorder.take();

        for id in ["pinned", "unknown"] {
            plugin.command(
                Command::DismissNotification { id: id.into() },
                &mut harness.ctx(),
            );
        }
        assert!(harness.sent().is_empty());
        assert!(harness.recorder.take().is_empty());

        plugin.command(
            Command::DismissNotification {
                id: "clearable".into(),
            },
            &mut harness.ctx(),
        );
        let sent = harness.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, TYPE_NOTIFICATION_REQUEST);
        assert_eq!(
            Value::Object(sent[0].body.clone()),
            json!({"cancel": "clearable"})
        );
        assert_eq!(
            harness.recorder.take(),
            [Event::NotificationRemoved("clearable".into())]
        );

        // The phone confirms; that is not reported twice.
        plugin.packet(&cancel("clearable"), &mut harness.ctx());
        assert!(harness.recorder.take().is_empty());
    }

    #[test]
    fn scans_title_and_text_for_codes() {
        assert_eq!(otp_text("Title", "Text"), "Title\nText");
        assert_eq!(otp_text("", "Text"), "Text");
        assert_eq!(otp_text("Title", ""), "Title");
        assert_eq!(otp_text("", ""), "");
    }

    #[test]
    fn scans_new_text_and_recent_silent_notifications_only() {
        let now = 1_700_000_000_000;
        let minute = 60_000;
        // New, or changed.
        assert!(wants_otp_scan(None, "a", false, now, now));
        assert!(wants_otp_scan(Some("a"), "b", false, now, now));
        // Unchanged, or nothing to scan.
        assert!(!wants_otp_scan(Some("a"), "a", false, now, now));
        assert!(!wants_otp_scan(None, "", false, now, now));
        // Silent: only if recent. Loud ones are new whatever the clock says.
        assert!(wants_otp_scan(None, "a", true, now - minute, now));
        assert!(!wants_otp_scan(None, "a", true, now - 60 * minute, now));
        assert!(wants_otp_scan(None, "a", false, now - 60 * minute, now));
        // A phone clock ahead of ours.
        assert!(wants_otp_scan(None, "a", true, now + minute, now));
    }

    #[test]
    fn records_the_scanned_text_per_notification() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        plugin.packet(&posted("a"), &mut harness.ctx());
        assert_eq!(
            lock(&plugin.state).live["a"].otp_text,
            "Sample Sender\nSample message"
        );
    }

    #[test]
    fn accepts_only_hex_icon_hashes() {
        assert_eq!(icon_name(HASH).as_deref(), Some(HASH));
        assert_eq!(icon_name("ABCDEF0123").as_deref(), Some("abcdef0123"));
        for bad in [
            "",
            "../../etc/passwd",
            "abc/def",
            "abc.png",
            "zz",
            &"a".repeat(129),
        ] {
            assert_eq!(icon_name(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn fetches_an_icon_once_and_posts_again_with_it() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut first = posted("a");
        first.body.insert("payloadHash".into(), HASH.into());
        let serving = offer(&harness, &mut first, png()).await;

        plugin.packet(&first, &mut harness.ctx());
        // Posted at once, without the icon.
        let events = harness.recorder.take();
        assert_eq!(posted_events(&events)[0].icon_path, None);

        serving.await.unwrap();
        until_no_downloads(&plugin).await;
        let file = icon_file(&harness.cache_dir, HASH);
        assert_eq!(std::fs::read(&file).unwrap(), png());
        let events = harness.recorder.take();
        let reposted = posted_events(&events);
        assert_eq!(reposted.len(), 1);
        assert_eq!(reposted[0].id, "a");
        assert_eq!(reposted[0].icon_path.as_deref(), Some(file.as_path()));
        assert!(reposted[0].silent);

        // The same icon again, offered or not: taken from the cache.
        let mut second = posted("b");
        second
            .body
            .insert("payloadHash".into(), HASH.to_uppercase().into());
        let serving = offer(&harness, &mut second, png()).await;
        plugin.packet(&second, &mut harness.ctx());
        assert!(lock(&plugin.state).downloading.is_empty());
        let events = harness.recorder.take();
        let posts = posted_events(&events);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].icon_path.as_deref(), Some(file.as_path()));
        assert!(!posts[0].silent);
        serving.abort();
    }

    #[tokio::test]
    async fn an_update_without_the_payload_gets_the_icon_being_downloaded() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut first = posted("a");
        first.body.insert("payloadHash".into(), HASH.into());
        let serving = offer(&harness, &mut first, png()).await;
        plugin.packet(&first, &mut harness.ctx());

        // The phone attaches an icon only the first time for an id.
        let mut update = posted("a");
        update.body.insert("payloadHash".into(), HASH.into());
        update.body.insert("text".into(), "Updated".into());
        plugin.packet(&update, &mut harness.ctx());
        harness.recorder.take();

        serving.await.unwrap();
        until_no_downloads(&plugin).await;
        let events = harness.recorder.take();
        let reposted = posted_events(&events);
        assert_eq!(reposted.len(), 1);
        assert_eq!(reposted[0].text, "Updated");
        assert!(reposted[0].icon_path.is_some());
    }

    #[tokio::test]
    async fn an_icon_arriving_after_the_removal_does_not_repost() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut first = posted("a");
        first.body.insert("payloadHash".into(), HASH.into());
        let serving = offer(&harness, &mut first, png()).await;
        plugin.packet(&first, &mut harness.ctx());
        // The test runtime is single-threaded: the download has not started.
        plugin.packet(&cancel("a"), &mut harness.ctx());
        harness.recorder.take();

        serving.await.unwrap();
        until_no_downloads(&plugin).await;
        assert!(harness.recorder.take().is_empty());
        // Still cached for next time.
        assert!(icon_file(&harness.cache_dir, HASH).is_file());
    }

    #[tokio::test]
    async fn a_bad_icon_never_loses_the_notification() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();
        let mut packet = posted("a");
        packet.body.insert("payloadHash".into(), HASH.into());
        let serving = offer(&harness, &mut packet, b"<html>not a png</html>".to_vec()).await;
        plugin.packet(&packet, &mut harness.ctx());

        serving.await.unwrap();
        until_no_downloads(&plugin).await;
        let events = harness.recorder.take();
        assert_eq!(posted_events(&events).len(), 1);
        assert_eq!(posted_events(&events)[0].icon_path, None);
        assert!(!icon_file(&harness.cache_dir, HASH).exists());
        assert!(lock(&plugin.state).live.contains_key("a"));
    }

    #[tokio::test]
    async fn refuses_oversized_and_unnamed_icons() {
        let mut harness = Harness::new();
        let mut plugin = Notifications::default();

        let mut huge = posted("huge");
        huge.body.insert("payloadHash".into(), HASH.into());
        let serving = offer(&harness, &mut huge, png()).await;
        huge.payload_size = Some(i64::try_from(MAX_ICON_BYTES).unwrap() + 1);
        plugin.packet(&huge, &mut harness.ctx());
        assert!(lock(&plugin.state).downloading.is_empty());
        serving.abort();

        let mut unnamed = posted("unnamed");
        unnamed
            .body
            .insert("payloadHash".into(), "../escape".into());
        let serving = offer(&harness, &mut unnamed, png()).await;
        plugin.packet(&unnamed, &mut harness.ctx());
        assert!(lock(&plugin.state).downloading.is_empty());
        serving.abort();

        let events = harness.recorder.take();
        let posts = posted_events(&events);
        assert_eq!(posts.len(), 2);
        assert!(posts.iter().all(|post| post.icon_path.is_none()));
    }
}
