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
| Build | **SwiftPM plus shell scripts; no Xcode project** | The owner's Mac is short on disk, so full Xcode is not installed. Everything builds, tests, and bundles with the Command Line Tools |
| Repo | **Open source.** The public home is `tuhin-cmd/land-line`. Work stays local until the owner says to publish | Owner's decision (2026-11-10) |
| License | **GPL-3.0-or-later** (owner, 2026-11-10) | KDE Connect is GPL, so its source can be read freely while verifying the protocol. Its files are "GPL-2.0-only OR GPL-3.0-only", so any file closely adapted from them must be marked `GPL-3.0-only`. So far everything is written from observed behaviour, not adapted |

## Out of scope (for now)

Screen mirroring, remote input, media control, telephony, running over the internet or a VPN, Windows or Linux UI, an own Android app, iPhone support.

## Status

| Milestone | State |
|---|---|
| M0: skeleton | **Done** (2026-11-10). Rust checks and Swift bridge tests pass, and `scripts/build-app.sh` produces a signed `build/Landline.app` whose menu reads "Landline core 0.1.0". |
| M1: discovery, TLS, pairing | Built (2026-11-10) and working against the real phone: the OnePlus 15 running stock KDE Connect was discovered, paired from the Mac with a matching verification key, and reconnects as paired under the pinned certificate in both the CLI and the app. The menu bar panel lists devices and handles pairing in both directions (`--demo` shows every state without a network). **Still owed for the exit check:** a ping from the phone seen on the Mac, the owner confirming the Mac's ping showed on the phone, and the link coming back after toggling Wi-Fi on each device. Also untested on the real phone: pairing started from the phone, and unpairing. |
| M2: notifications and OTP | Core and bridge built (2026-11-10): notifications with cached icons, battery, SMS, one-time code extraction (70-case table in English and Bangla). Unit-tested against packet shapes from the Android source; **not yet seen with real notifications or a real OTP SMS** (the phone had not granted KDE Connect notification access when last checked). |
| M3: clipboard | Core and bridge built. Covered by the two-device loopback test. Not yet tried with the phone. |
| M4: files | Core and bridge built: receive into the download folder with safe names, send with per-file ports, progress and cancel. A file crosses between two cores in the loopback test. Not yet tried with the phone. |
| M5: storage | `crates/landline-sftp` (russh client, tested against an in-process server and macOS's own sshd) plus the storage plugin in the core. Not yet tried with the phone. |
| M6: polish | In progress. |

Notes from M0 that later milestones should know:

- **Remotes.** `origin-1` is the public `tuhin-cmd/land-line`, the project's eventual home; it holds only the initial commit. `origin` is a private `tuhin-cmd/landline` that received the first M0 commits and is where `main` currently tracks. Do not push to either without the owner asking.
- **No Xcode.** The app is a SwiftPM executable (`apple/Package.swift`); `scripts/build-app.sh` wraps it in a bundle with `apple/Support/Info.plist` and signs it ad hoc. `scripts/test-swift.sh` adds the flags the Command Line Tools need to find Swift Testing. Things that may force the question later: notarization in M6 (`notarytool` ships with Xcode; check whether a standalone tool such as `rcodesign` covers it), and app extensions (Share, File Provider), which need their own bundles and entitlements.
- **Disk.** The Mac had about 6 GB free during M0. Scripts build a debug app for the host architecture by default; `--release --universal` is for CI and releases. Debug info for dependencies is off in the dev profile to keep `target/` small.
- **The ad-hoc signature changes with every build,** so macOS may ask again for permissions (Local Network, notifications) after a rebuild. A stable signing identity fixes that; a self-signed certificate would do before Developer ID.
- **The xcframework is assembled by hand** in `scripts/build-xcframework.sh` (static library, header, module map, `Info.plist`) instead of with `xcodebuild -create-xcframework`.
- **UniFFI async and callbacks work (risk 7).** M0 proved an async method and a callback from a core thread; both placeholders are gone now. Async methods can spawn onto the core's own runtime and await the join handle, so UniFFI's `tokio` feature is not needed. The M1 surface is synchronous: commands return at once and results arrive through `CoreDelegate`.
- **`CoreDelegate` is a UniFFI callback interface** (`Box<dyn CoreDelegate>`), which the core holds for its lifetime. A Swift delegate that holds a strong reference to `LandlineCore` creates a retain cycle.

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
│   ├── build-xcframework.sh    builds the core as a static library, generates Swift bindings
│   ├── build-app.sh            builds build/Landline.app (calls build-xcframework.sh)
│   └── test-swift.sh           runs the Swift tests
└── apple/
    ├── Package.swift           SwiftPM package: the app and LandlineKit
    ├── Support/Info.plist      bundle metadata, copied into the app by build-app.sh
    ├── Sources/LandlineKit/    generated Swift bindings to the XCFramework
    ├── Sources/Landline/       SwiftUI app
    └── Tests/
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

Checked against the `KDE/kdeconnect-android` source (commit `207dcdb`, 2026-11-08) and, where noted, `KDE/kdeconnect-kde`.

**Packets.** One JSON object per line, newline terminated: `{"id": <ms since epoch>, "type": "kdeconnect.<name>", "body": {…}}`. Optional `payloadSize` (-1 means unknown) and `payloadTransferInfo` (`{"port": N}`); both are omitted, never `null`, when there is no payload, and a packet with `payloadSize` but no `payloadTransferInfo` is rejected. Peers read body values leniently (numbers as strings and the reverse), so we do too.

**Identity.** Body: `deviceId`, `deviceName`, `deviceType` (phone, tablet, desktop, laptop, tv), `protocolVersion`, `incomingCapabilities`, `outgoingCapabilities`, plus `tcpPort` in the UDP copy, and `targetDeviceId` and `targetProtocolVersion` in the plaintext TCP copy. A device ID is 32 to 38 characters of `[a-zA-Z0-9_-]`. A device name loses the characters `"',;:.!?()[]<>` and is cut to 32.

**Discovery.** Each device sends its identity as a UDP datagram to port 1716 on start and on network change, and advertises over mDNS as `_kdeconnect._udp` (instance name = device ID; TXT `id`, `name`, `type`, `protocol`). The Android app answers an mDNS sighting by sending its UDP identity to that host rather than connecting. It ignores anything from a non-private address, and a second identity from the same device or IP within a second. On macOS 26 a send to 255.255.255.255 fails with "No route to host" even with the Local Network permission; the per-interface directed broadcast is what works.

**Link setup, including the role inversion.** The device that *receives* a UDP identity opens a TCP connection to the sender's `tcpPort` (1716-1764) and writes its own identity as one plaintext line. TLS then starts with the roles reversed: **the side that opened the TCP connection is the TLS server**. In protocol 8, both sides then send their identity again inside the TLS session and only that copy is trusted; the device ID and protocol version must match the plaintext copy. A paired device offering an older protocol version than it used before is refused. There is no keepalive packet: a dead peer is noticed only through TCP keepalive, and a link comes back only when one side announces again.

**Certificates.** Each device has a long-lived self-signed certificate: subject `CN=<device id>, OU=KDE Connect, O=KDE`, EC P-256 on current Android installs (RSA 2048 on old ones), no extensions. Trust is on first use: before pairing any certificate is accepted; pairing pins the peer's certificate and later connections must present exactly it. The Android app only speaks TLS 1.2. Both sides present a certificate in every handshake. We also require the certificate's common name to equal the claimed device ID, as the desktop app does.

**Pairing.** Request: `kdeconnect.pair` `{"pair": true, "timestamp": <unix seconds>}`. Accept: `{"pair": true}`. Reject, cancel or unpair: `{"pair": false}`. The requester gives up after 30 s, the asked side after 25 s. The timestamp must be within 30 minutes of the receiver's clock. A device that receives any other packet from an unpaired peer answers with `{"pair": false}`, which is how two sides that disagree get back in step. **Verification key:** SHA-256 over the two certificates' `SubjectPublicKeyInfo` DER (the bytewise larger one first) followed by the timestamp in decimal ASCII; the first 8 hex characters, uppercased.

**Payloads.** The sender listens on a port from 1739 up and names it in `payloadTransferInfo`. The receiver connects to it. Here the roles are *not* inverted: the sender (TCP listener) is the TLS server.

**Packet types by feature.** All checked against the Android source.

| Feature | Types and behaviour |
|---|---|
| Notifications | `kdeconnect.notification`, phone to Mac: `id`, `appName`, `time` (ms, as a string), `isClearable`, `silent`, `ticker`, and `title` and `text` only if the app set them; `{id, isCancel: true}` for removal. The app icon is a PNG payload named by `payloadHash` (MD5), attached only the first time that hash goes out on a connection. `kdeconnect.notification.request`, Mac to phone: `{"request": true}` resends everything with `silent: true`; `{"cancel": id}` dismisses. The phone also pushes its current notifications unprompted, silent, when its plugin starts. |
| Battery | `kdeconnect.battery`, phone to Mac, unprompted on connect and on change: `currentCharge`, `isCharging`, `thresholdEvent`. There is no request packet. |
| SMS | `kdeconnect.sms.messages`, phone to Mac: `{version: 2, messages: [...]}`, each with `addresses`, `body`, `date` (ms), `type` (1 = inbox), `read`, `thread_id`, `_id`, `event` (bit 1 = has text). **The phone pushes new messages only after we have sent a request on that connection**, so on every connect we send `kdeconnect.sms.request_conversation` for thread -1 (matches nothing, answers empty) and repeat it every five minutes, because the phone recreates its plugin and forgets. Each push carries everything from the newest message it had seen, so the same message arrives repeatedly; we report a message once, and only if it is newer than the connection. |
| Clipboard | `kdeconnect.clipboard` `{content}` on every change; `kdeconnect.clipboard.connect` `{content, timestamp}` once when a link comes up, where the newer clipboard wins. We never send the connect packet, because the core does not hold the Mac's clipboard. |
| Share | `kdeconnect.share.request`: `{text}`, or `{url}`, or `{filename, lastModified, numberOfFiles, totalPayloadSize, open}` with the bytes as payload (`payloadSize` -1 if unknown; an empty file has no payload). `kdeconnect.share.request.update` `{numberOfFiles, totalPayloadSize}` when files join a running batch. The phone sends one file at a time, each on a fresh port. When receiving, it waits **one second** after each file for the next packet and fails the whole batch otherwise, so we send files strictly one after another. |
| Storage | `kdeconnect.sftp.request` `{startBrowsing: true}`, answered by `kdeconnect.sftp`: `port`, `user` (always `kdeconnect`), `password` (random, per request), `path`, `multiPaths`, `pathNames`; or `{errorMessage}`. The `ip` field is the phone's guess and is ignored in favour of the link's address. **The server's SSH host key is the phone's KDE Connect identity key**, so it is checked against the certificate pinned at pairing. |
| Ping | `kdeconnect.ping`, either way, empty body. |

Capabilities the Mac does not list in its identity are never sent to it, so advertise only what is implemented.

## Milestones

Each milestone ends with something demonstrable on the real phone. Do not start the next one until the exit check passes.

### M0: skeleton

- Cargo workspace with the three crates; `cargo test`, `cargo clippy`, `cargo fmt --check` clean.
- One UniFFI function (`core_version()`) exported and called from Swift.
- `scripts/build-xcframework.sh`: build the core for one or both Mac architectures, `lipo`, generate Swift bindings, assemble `LandlineFFI.xcframework`.
- SwiftPM package and `scripts/build-app.sh`; menu bar app that shows the core version.
- CI on GitHub Actions: Rust checks on Linux, full build on macOS.

**Exit:** `./scripts/build-app.sh --run` from a clean checkout produces an app whose menu shows the core version.

### M1: discovery, TLS, pairing

- Packet type with serde, tolerant of string `id`s.
- Identity and certificate generation, persisted under Application Support (private key file mode 0600; Keychain is a later improvement).
- UDP broadcast send and receive, mDNS, TCP listener, inverted TLS handshake, protocol 8 identity re-exchange.
- Pairing state machine and pinned-certificate verification.
- Reconnect on network change, wake from sleep, and phone IP change.
- CLI: `landline discover`, `landline pair <device>`, `landline listen` (dumps every packet as JSON).
- Swift: device list, pair and unpair, verification key prompt.

**Exit:** The Mac pairs with the stock KDE Connect app, a ping works in both directions, and the link comes back by itself after toggling Wi-Fi on either device.

**Do the OTP spike here** (see Risks, item 1), because `landline listen` is all it needs. It does need the Mac to advertise the notification and SMS packet types first: the phone sends nothing we do not list in our identity, and M1 lists only ping.

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

1. **Android redacts OTP notifications (highest risk).** *Update: the SMS route is built and does not depend on notifications; see the SMS row in the packet table for what it needs. The phone must grant KDE Connect the SMS, phone and notification permissions.* Since Android 15, notifications the system classifies as carrying a one-time code are redacted for ordinary notification-listener apps. The plan therefore relies on the SMS plugin, which reads SMS directly, for SMS codes. Codes delivered only as app notifications (banking apps, email) may be unreachable. **Spike in M1:** trigger real OTPs and record exactly what `kdeconnect.notification` and `kdeconnect.sms.messages` contain. Also confirm the SMS plugin pushes new messages unprompted. The result decides how M2 is built.
2. **Android blocks background clipboard reads.** *Confirmed: after copying on the phone, the user taps "Send clipboard" in KDE Connect or its quick-settings tile. Granting `READ_LOGS` over ADB makes it automatic.* Since Android 10, an app not in the foreground cannot read the clipboard, so phone-to-Mac sync cannot be fully automatic on a stock phone. Options, in order of preference: the KDE Connect "send clipboard" action or quick-settings tile; granting `READ_LOGS` over ADB (a workaround KDE Connect supports); an own Android fork with a different trigger. Pick after trying the first.
3. **TLS interoperability.** The inverted roles, self-signed certificates, and whatever key types and TLS versions the Android app offers must all work with `rustls` behind custom verifiers. If `rustls` cannot be made to interoperate, fall back to the `openssl` crate rather than fighting it.
4. **SFTP interoperability.** *Update: the phone's server (Apache SSHD) offers ECDH on the NIST curves, group14, AES-CTR/GCM and SHA-2 MACs with an ECDSA P-256 or RSA host key; the client enables the ones russh leaves off. Android 11+ needs "All files access" for KDE Connect; older versions need folders added in the plugin's settings.* The phone's SFTP server is an embedded Java SSH server that may offer only older key-exchange or host-key algorithms. `russh` may need them enabled explicitly. Also, Android's storage rules limit what the server can expose; the phone app may need "All files access" or folders added in its SFTP settings.
5. **OxygenOS battery management.** The phone app must be exempt from battery optimization and allowed to auto-launch, or the link dies with the screen off. Document the exact toggles once found.
6. **macOS Local Network privacy.** Confirmed in practice, and subtler than expected: without the permission, broadcasts still leave the Mac and devices can still connect *to* it, so the phone appears and pairing works. What fails is every connection the Mac opens to the phone: answering the phone's announcement, fetching a file or icon, SFTP, and announcing to a known address. Each fails at once with "No route to host". The core treats that as `NetworkStatus::Blocked`. The permission belongs to the app that owns the process: for the CLI that is the terminal it runs in.
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

1. ~~License.~~ Decided: GPL-3.0-or-later.
2. **OTP auto-copy** on by default, or opt-in behind the notification action?
3. **Intel Macs:** build universal, or Apple silicon only?
4. Is a fork of the Android app acceptable later if clipboard or OTP behaviour demands it?
