<img src="assets/icon.png" width="64" height="64" alt="">

# landline

A macOS menu bar app that links to an Android phone over the local network, through the stock KDE Connect app: notifications and SMS one-time codes, clipboard sync, and files. No cloud, no account, no relay server.

Work in progress. See [PLAN.md](PLAN.md) for the design, the protocol notes, and what has and has not been tested against a real phone.

## What it does

- **Devices:** finds phones running KDE Connect on the same network, pairs with a verification key, and reconnects by itself.
- **Notifications:** mirrors the phone's notifications as Mac notifications, with the app's icon. Dismissing one on the Mac dismisses it on the phone. Apps can be muted.
- **One-time codes:** a code arriving by SMS or notification is offered with a Copy action, and copied automatically if you leave that setting on.
- **Clipboard:** text copied on the Mac goes to the phone. Text copied on the phone reaches the Mac after you tap "Send clipboard" in KDE Connect (Android does not let apps read the clipboard in the background).
- **Files:** send files to the phone from a device's menu or by dropping them on its row; files, text and links shared from the phone arrive on the Mac.
- **Phone storage:** browse the phone's folders in a window, preview with Quick Look, download and upload.
- **Battery:** the phone's charge shows next to its name.

## Set up the phone

1. Install KDE Connect (the F-Droid build has every plugin) and join the same Wi-Fi as the Mac.
2. Open the Mac's entry in KDE Connect and grant what its plugins ask for: notification access, SMS and phone, and files ("All files access" on Android 11 and later). If Android greys a permission out for a sideloaded build, open the app's info page, the three-dot menu, and choose "Allow restricted settings".
3. Exempt KDE Connect from battery optimisation and allow it to run in the background, or the link drops when the screen turns off.

## Set up the Mac

macOS asks for three things, each once: to send notifications, to access the Downloads folder (when the first file arrives from the phone; the file waits for your answer), and to find devices on your local network.

The first time the app tries to reach the phone, macOS asks whether Landline may find devices on your local network. Allow it: without it the phone still appears, but files, storage and reconnecting fail, and the panel says so. The setting is under System Settings > Privacy & Security > Local Network.

## Build

Needs Rust (via rustup) and the Xcode Command Line Tools (`xcode-select --install`). Full Xcode is not required.

```sh
./scripts/build-app.sh --run
```

This builds `build/Landline.app` and launches it. Look for the phone icon in the menu bar.

macOS ties the Local Network and notification permissions to the app's signature, and the default ad-hoc signature changes with every build, so a rebuilt app has to be allowed again. To avoid that, create a code signing certificate once (Keychain Access > Certificate Assistant > Create a Certificate, type "Code Signing") and build with its name:

```sh
LANDLINE_SIGN_IDENTITY="My Certificate Name" ./scripts/build-app.sh --run
```

`./scripts/build-app.sh` then `open build/Landline.app --args --demo` shows the interface with made-up devices and no network.

### Command line

`cargo run -p landline-cli -- <command>` drives the same core without the app: `discover`, `pair`, `unpair`, `ping`, `listen`, `send`, `clipboard`, `ls`, `get`, `put`. Add `-vv` for logs. It shares the app's identity, so quit the app first. The terminal you run it in needs the Local Network permission too.

### Tests

```sh
cargo test --workspace
./scripts/test-swift.sh
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

Landline speaks the KDE Connect protocol but is a separate project, not affiliated with KDE.
