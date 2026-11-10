<img src="assets/icon.svg" width="64" height="64" alt="">

# landline

A macOS menu bar app that links to an Android phone over the local network, through the stock KDE Connect app: notifications and SMS one-time codes, clipboard sync, and files. No cloud, no account, no relay server.

Early work in progress. See [PLAN.md](PLAN.md) for the design and milestones.

## Build

Needs Rust (via rustup), Xcode, and [XcodeGen](https://github.com/yonaskolb/XcodeGen).

```sh
./scripts/build-xcframework.sh
cd apple && xcodegen generate
xcodebuild build -project Landline.xcodeproj -scheme Landline
```
