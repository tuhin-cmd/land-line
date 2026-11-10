# Landline: project context and implementation plan

This file is the handoff for a Claude Code session. Read it fully before doing anything. It records what has been decided, what is still open, and the order to build things in. Save it in the repo as `PLAN.md` and keep it current as milestones close.

## Goal

A macOS menu bar app that pairs with an Android phone (OnePlus 15, OxygenOS 16 / Android 16) over the local network and does three things well:

1. **Notifications and SMS one-time codes.** Phone notifications appear on the Mac. When an SMS or notification carries an OTP, the code is one click (or zero clicks) from the Mac clipboard.
2. **Clipboard sync.** Text copied on one device can be pasted on the other.
3. **Files.** Send files in both directions, and browse the phone's storage from the Mac.

LAN only. No cloud, no account, no relay server.

## Decisions already made

| Topic | Decision | Why |
|---|---|---|
| Name | `landline` (working name: a phone link that only works on the LAN) | Change freely if the repo name is taken or the owner prefers another |
| Protocol | KDE Connect protocol, version 8 | Reuses a mature, documented protocol instead of inventing one |
| Android side | Stock **KDE Connect Android app**, unmodified | It already solves background services, permissions, pairing, SMS, and SFTP. Fork only if a milestone proves it necessary |
| Mac side | **Rust core + SwiftUI app**, bridged with UniFFI | Owner's choice. Core stays portable, UI stays native |
| Distribution | Non-sandboxed, Developer ID signed, outside the App Store | Sandbox complicates LAN broadcast, SFTP, and file access for no benefit to a personal tool |
| Minimum macOS | 14 | `MenuBarExtra` and `@Observable` |
| Repo visibility | Private to start | Owner can open it later |
| License | **Not chosen yet** | See "Open questions" |

## Out of scope (for now)

Screen mirroring, remote input, media control, telephony, running over the internet or a VPN, Windows or Linux UI, an own Android app, iPhone support.

## Status

| Milestone | State |
|---|---|
| M0: skeleton | Built and passing in CI (2026-11-10). One check is still owed: nobody has yet launched the app and seen the version in the menu, because the development Mac has only the Command Line Tools. Install Xcode, run the Build steps in `README.md`, and open the menu to close M0. |
| M1 onward | Not started |

Notes from M0 that later milestones should know:

- **Repo.** `origin` is the private `tuhin-cmd/landline`. A second remote, `origin-1`, points at a public `tuhin-cmd/land-line` that holds only the initial commit. Nothing is pushed there; the owner should decide whether to delete it.
- **The xcframework is assembled by hand** in `scripts/build-xcframework.sh` (static library, header, module map, `Info.plist`) instead of with `xcodebuild -create-xcframework`, so the script runs without full Xcode. CI builds the app from it with real Xcode.
- **UniFFI async and callbacks work (risk 7).** `LandlineCore.echo_after` (async) and `CoreDelegate.heartbeat` (callback from a core thread) are placeholders that prove the pattern, covered by `apple/LandlineKit/Tests`. Async methods spawn onto the core's own runtime and await the join handle, so UniFFI's `tokio` feature is not needed. Remove both placeholders when M1 adds real methods.
- **`CoreDelegate` is a UniFFI callback interface** (`Box<dyn CoreDelegate>`), which the core holds for its lifetime. A Swift delegate that holds a strong reference to `LandlineCore` creates a retain cycle.
- **`swift test` needs full Xcode.** With only the Command Line Tools the `Testing` module is not found.

## Architecture

```
┌──────────────────────────── macOS ────────────────────────────┐
│  SwiftUI app (menu bar)                                       │
│   Notification Center · NSPasteboard · Finder drag and drop   │
│            │  async calls            ▲  delegate callbacks    │
│  ──────────┼──── UniFFI (landline-ffi) ────┼────────────────  │
│            ▼                         │                        │
│  Rust core (landline-core, tokio runtime)                     │
│   discovery · TLS links · pairing · plugins · SFTP client     │
└───────────────────────────────┬───────────────────────────────┘
                                │ LAN: UDP 1716, TCP 1716-1764
                     KDE Connect app on the phone
```

Rules for the boundary:

- **The core owns the protocol and all network state.** It never calls a macOS API.
- **Swift owns everything the OS provides:** pasteboard, notifications, Keychain, login item, file pickers, permissions.
- **`landline-ffi` is thin.** It adapts core types for UniFFI and holds no logic.
- **Every feature works from the CLI first.** `landline-cli` drives the core headlessly, so protocol work can be tested against the real phone without touching Swift.

### Repo layout

```
landline/
├── PLAN.md                     this file
├── CLAUDE.md                   short working notes for Claude Code (commands, conventions)
├── Cargo.toml                  workspace
├── crates/
│   ├── landline-core/          protocol, transport, plugins, OTP extraction
│   ├── landline-ffi/           UniFFI exports + uniffi-bindgen binary
│   └── landline-cli/           `landline` binary: discover, pair, listen, send, ls
├── scripts/
│   └── build-xcframework.sh    builds both Mac targets, lipo, generates Swift bindings
└── apple/
    ├── project.yml             XcodeGen spec (keeps the Xcode project text-diffable)
    ├── LandlineKit/            Swift package wrapping the XCFramework
    └── Landline/               SwiftUI app
```

### Suggested crates

`tokio`, `rustls` + `tokio-rustls` (custom certificate verifiers for pinning), `rcgen` (self-signed cert), `socket2` (UDP broadcast), `mdns-sd`, `serde` / `serde_json`, `russh` + `russh-sftp`, `uniffi`, `regex`, `thiserror`, `tracing`, `directories`. Check current versions with `cargo add`; do not pin from memory.

### FFI surface (sketch, refine in M0 and M1)

```
object LandlineCore
  new(config, delegate)
  start() / stop() / network_changed()
  devices() -> [Device]
  request_pair(id) / accept_pair(id) / reject_pair(id) / unpair(id)
  send_clipboard(text)
  send_files(id, paths)
  dismiss_notification(id, notification_id)
  list_dir(id, path) async -> [RemoteEntry]
  download(id, remote, local) async / upload(id, local, remote) async

callback interface CoreDelegate
  devices_changed([Device])
  pair_requested(Device, verification_key)
  notification_posted(device_id, PhoneNotification)
  notification_removed(device_id, notification_id)
  otp_detected(OtpCode)            code, source app or sender, device
  clipboard_received(text)
  transfer_progress(Transfer)
```

The core runs its own tokio runtime. Delegate callbacks arrive on core threads; Swift hops to `@MainActor`.

## Protocol notes

**These are written from memory. Treat every line as a claim to verify** against the Valent protocol reference (clearest write-up) and the `KDE/kdeconnect-android` and `KDE/kdeconnect-kde` sources before building on it.

**Packets.** One JSON object per line, newline terminated: `{"id": <ms since epoch>, "type": "kdeconnect.<name>", "body": {…}}`. Optional `payloadSize` (-1 means unknown) and `payloadTransferInfo` (`{"port": N}`). Some peers send `id` as a string, so accept both.

**Discovery.** Each device broadcasts its identity packet over UDP to port 1716. Newer versions also advertise over mDNS as `_kdeconnect._udp`. Identity body: `deviceId`, `deviceName`, `deviceType`, `protocolVersion`, `incomingCapabilities`, `outgoingCapabilities`, `tcpPort`.

**Link setup, including the role inversion.** The device that *receives* a UDP identity opens a TCP connection to the sender's `tcpPort` (range 1716-1764) and writes its own identity as one plaintext line. TLS then starts with the roles reversed: **the side that opened the TCP connection acts as the TLS server**, and the side that accepted it acts as the TLS client. In protocol 8, both sides send their identity again inside the TLS session, and only that copy is trusted.

**Certificates and pairing.** Each device has a long-lived self-signed certificate whose common name is its device ID. Trust is on first use: pairing pins the peer's certificate, and later connections must present the same one. Protocol 8 constrains the device ID format (UUID-like, underscores rather than hyphens) and adds a timestamp to `kdeconnect.pair`. Both sides show a short verification key derived from the two public keys. Get the exact derivation from source.

**Payloads.** The sender listens on a port in 1739-1764 and names it in `payloadTransferInfo`. The receiver connects to it over TLS and reads `payloadSize` bytes. Check which side is the TLS server here; it may not follow the inverted rule above.

**Packet types by feature.**

| Feature | Types |
|---|---|
| Notifications | `kdeconnect.notification` (phone to Mac; `id`, `appName`, `title`, `text`, `ticker`, `isCancel`, `isClearable`, `time`, icon as payload), `kdeconnect.notification.request` (`{"request": true}` to fetch all, `{"cancel": id}` to dismiss), `.reply`, `.action` |
| SMS | `kdeconnect.sms.messages` (phone to Mac), `kdeconnect.sms.request_conversations`, `kdeconnect.sms.request_conversation` |
| Clipboard | `kdeconnect.clipboard` (`content`), `kdeconnect.clipboard.connect` (`content`, `timestamp`) |
| File send | `kdeconnect.share.request` (`filename`, or `text`, or `url`; file bytes as payload), `kdeconnect.share.request.update` |
| File browse | `kdeconnect.sftp.request` (`{"startBrowsing": true}`), answered by `kdeconnect.sftp` (`ip`, `port`, `user`, `password`, `path`, `multiPaths`, `pathNames`) |
| Health | `kdeconnect.ping`, `kdeconnect.battery` |

Capabilities the Mac does not list in its identity are never sent to it, so advertise only what is implemented.

## Milestones

Each milestone ends with something demonstrable on the real phone. Do not start the next one until the exit check passes.

### M0: skeleton

- Cargo workspace with the three crates; `cargo test`, `cargo clippy`, `cargo fmt --check` clean.
- One UniFFI function (`core_version()`) exported and called from Swift.
- `scripts/build-xcframework.sh`: build `aarch64-apple-darwin` and `x86_64-apple-darwin`, `lipo`, generate Swift bindings, assemble `LandlineFFI.xcframework`.
- XcodeGen project; menu bar app that shows the core version.
- CI on GitHub Actions: Rust checks on Linux, full build on macOS.

**Exit:** `./scripts/build-xcframework.sh && xcodebuild` from a clean checkout produces an app whose menu shows the core version.

### M1: discovery, TLS, pairing

- Packet type with serde, tolerant of string `id`s.
- Identity and certificate generation, persisted under Application Support (private key file mode 0600; Keychain is a later improvement).
- UDP broadcast send and receive, mDNS, TCP listener, inverted TLS handshake, protocol 8 identity re-exchange.
- Pairing state machine and pinned-certificate verification.
- Reconnect on network change, wake from sleep, and phone IP change.
- CLI: `landline discover`, `landline pair <device>`, `landline listen` (dumps every packet as JSON).
- Swift: device list, pair and unpair, verification key prompt.

**Exit:** The Mac pairs with the stock KDE Connect app, a ping works in both directions, and the link comes back by itself after toggling Wi-Fi on either device.

**Do the OTP spike here** (see Risks, item 1), because `landline listen` is all it needs.

### M2: notifications and OTP

- Notification plugin: post, update, remove, dismiss from the Mac, icons via payload.
- SMS plugin: receive new messages as they arrive.
- OTP extraction in the core (rules below), with a unit test per real message format the owner provides.
- Swift: `UNUserNotificationCenter` banners with a "Copy code" action; a setting for auto-copy; per-app mute list.
- Write OTPs to the pasteboard with the `org.nspasteboard.ConcealedType` marker so clipboard managers skip them.

**OTP rules.** Require an OTP keyword in the message (English plus Bangla: code, OTP, verification, PIN, কোড, ওটিপি, পিন). Take the 4 to 8 digit run nearest the keyword. Accept one split ("123 456", "123-456"). Reject runs that are amounts (next to Tk, BDT, ৳, $, and so on), decimals, or part of a longer number such as a phone or account number. A draft of this with passing tests exists from the planning chat; ask the owner for it or rewrite it from these rules.

**Exit:** A real OTP SMS sent to the phone is on the Mac clipboard within about two seconds, and a bank balance SMS without a code is not misread as one.

### M3: clipboard

- Mac to phone: Swift polls `NSPasteboard.changeCount` (about every 500 ms) and calls `send_clipboard`.
- Phone to Mac: receive `kdeconnect.clipboard`, write to the pasteboard.
- Loop prevention: remember the hash of the last value received and do not send it back.
- Skip concealed and transient pasteboard items (password managers).

**Exit:** Copy on the Mac and paste on the phone with no action. Copy on the phone and reach the Mac with one explicit action (see Risks, item 2).

### M4: file send and receive

- Receive `share.request` payloads into a configurable folder (default `~/Downloads`), handling multi-file batches and name collisions.
- Send files from the Mac: drag onto the menu bar icon, a file picker, and later a Share extension.
- Shared text and URLs: open or copy.
- Progress and cancel through `transfer_progress`.

**Exit:** A 1 GB file transfers in each direction with correct size and checksum, and cancelling mid-transfer leaves no partial file behind.

### M5: browse phone storage

- SFTP client in the core with `russh-sftp`, using the credentials from `kdeconnect.sftp`.
- `list_dir`, `download`, `upload`, plus delete and rename if cheap.
- Swift: a browser window with a column or list view, Quick Look, and drag out to Finder.
- Stretch goal after everything else works: a File Provider extension so the phone appears in Finder's sidebar.

**Exit:** Browse to `DCIM/Camera`, preview a photo, drag a video to the Desktop.

### M6: daily-driver polish

- Launch at login (`SMAppService`), Local Network permission flow (`NSLocalNetworkUsageDescription`, `NSBonjourServices`), signing and notarization.
- Settings window, connection status in the menu, logs export.
- A week of real use, then fix what was annoying.

## Risks and spikes

1. **Android redacts OTP notifications (highest risk).** Since Android 15, notifications the system classifies as carrying a one-time code are redacted for ordinary notification-listener apps. The plan therefore relies on the SMS plugin, which reads SMS directly, for SMS codes. Codes delivered only as app notifications (banking apps, email) may be unreachable. **Spike in M1:** trigger real OTPs and record exactly what `kdeconnect.notification` and `kdeconnect.sms.messages` contain. Also confirm the SMS plugin pushes new messages unprompted. The result decides how M2 is built.
2. **Android blocks background clipboard reads.** Since Android 10, an app not in the foreground cannot read the clipboard, so phone-to-Mac sync cannot be fully automatic on a stock phone. Options, in order of preference: the KDE Connect "send clipboard" action or quick-settings tile; granting `READ_LOGS` over ADB (a workaround KDE Connect supports); an own Android fork with a different trigger. Pick after trying the first.
3. **TLS interoperability.** The inverted roles, self-signed certificates, and whatever key types and TLS versions the Android app offers must all work with `rustls` behind custom verifiers. If `rustls` cannot be made to interoperate, fall back to the `openssl` crate rather than fighting it.
4. **SFTP interoperability.** The phone's SFTP server is an embedded Java SSH server that may offer only older key-exchange or host-key algorithms. `russh` may need them enabled explicitly. Also, Android's storage rules limit what the server can expose; the phone app may need "All files access" or folders added in its SFTP settings.
5. **OxygenOS battery management.** The phone app must be exempt from battery optimization and allowed to auto-launch, or the link dies with the screen off. Document the exact toggles once found.
6. **macOS Local Network privacy.** Without the permission the app silently sees no devices. Detect that state and explain it in the UI rather than showing an empty list.
7. **UniFFI async and callbacks into Swift.** Prove the pattern end to end in M0 with a trivial async function and one callback before building real features on it.

## Phone setup checklist

- Install KDE Connect (the F-Droid build is the safest bet for having the SMS plugin; verify whichever build is used).
- Grant notification access, SMS, and storage permissions. If Android greys these out for a sideloaded build: App info, three-dot menu, "Allow restricted settings".
- Exempt the app from battery optimization and allow background activity.
- In the app: enable the Notification sync, SMS, Clipboard, Share, and Filesystem expose plugins.

## Working conventions

- Small commits, one concern each. `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` pass before every commit.
- Protocol code is tested against captured packets. When `landline listen` shows a new packet shape from the phone, save it as a fixture.
- Never commit real SMS content, phone numbers, device certificates, or keys. Scrub fixtures.
- Log with `tracing`. Never log notification bodies, SMS text, clipboard contents, or OTPs above `trace` level.
- When this plan and reality disagree, update the plan in the same commit.

## Open questions for the owner

1. **License.** KDE Connect's apps are GPL. Code written independently against the protocol can carry any license; code copied or closely adapted from those apps cannot. Decide the license before reading their source closely, and get proper advice if this may become a commercial product.
2. **OTP auto-copy** on by default, or opt-in behind the notification action?
3. **Intel Macs:** build universal, or Apple silicon only?
4. Is a fork of the Android app acceptable later if clipboard or OTP behaviour demands it?
