<img src="assets/icon.svg" width="64" height="64" alt="">

# landline

A macOS menu bar app that links to an Android phone over the local network, through the stock KDE Connect app: notifications and SMS one-time codes, clipboard sync, and files. No cloud, no account, no relay server.

Early work in progress. See [PLAN.md](PLAN.md) for the design and milestones.

## Build

Needs Rust (via rustup) and the Xcode Command Line Tools (`xcode-select --install`). Full Xcode is not required.

```sh
./scripts/build-app.sh --run
```

This builds `build/Landline.app` and launches it. Look for the phone icon in the menu bar.
