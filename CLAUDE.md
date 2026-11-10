# Landline

macOS menu bar app that links to an Android phone through the stock KDE Connect app. `PLAN.md` holds the decisions, milestones, and risks. Read it first, and update it in the same commit when it and reality disagree.

## Commands

```sh
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo run -p landline-cli -- discover    # drive the core headlessly; also pair, ping, listen, send, clipboard, ls, get, put (-vv for logs)

./scripts/build-app.sh --run             # Rust core + Swift app -> build/Landline.app, then relaunch it
./scripts/test-swift.sh                  # Swift tests; needs the xcframework from a build first
open -n build/Landline.app --args --demo # the UI with made-up devices and no network (--demo=files, =notifications, ...)
```

Both scripts build debug for this Mac's architecture. `--release --universal` is what CI builds.

There is no Xcode project and full Xcode is not installed: the Command Line Tools are enough. Do not add a step that needs `xcodebuild`. Disk has been short on this Mac; `cargo clean` and `rm -rf apple/.build` get space back.

`apple/LandlineFFI.xcframework`, `apple/Sources/LandlineKit/Generated`, and `build/` are build outputs and are not committed.

## Testing against the phone

- The app and the CLI share one identity (`~/Library/Application Support/io.github.tuhin-cmd.landline`). Never run both at once; quit the app before using the CLI.
- **Local Network permission.** macOS refuses connections to LAN devices from an app without it, with an immediate "No route to host", while still letting the phone connect in. The permission belongs to the app that owns the process, so the CLI run from a terminal or an agent session needs that host app allowed, and a rebuilt ad-hoc-signed Landline.app has to be allowed again. If outbound connections fail, this is why; only the owner can grant it.
- The core's log is at `~/Library/Logs/Landline/landline.log` when the app runs.
- `landline listen` prints packet bodies, which hold the owner's real notifications and messages. Summarise types and counts; do not print or keep the contents.

## Layout

- `crates/landline-core`: protocol and state. `manager.rs` is the single task that owns devices and links; `link.rs` is the handshake; `plugins/` has one file per feature (see `plugins/mod.rs` for the `Plugin` trait and the test harness); `payload.rs` moves files and icons.
- `crates/landline-sftp`: SFTP client for the phone's storage server.
- `crates/landline-ffi`: UniFFI surface. `crates/landline-cli`: the `landline` binary.
- `apple/Sources/Landline`: the app. `AppModel` mirrors the core; `CoreBackend` is the real backend and `DemoBackend` the scripted one; OS services sit behind protocols in `AppServices.swift` so logic is testable.

## Conventions

- The core owns the protocol and network state and never calls a macOS API. Swift owns everything the OS provides. `landline-ffi` adapts types and holds no logic.
- Every feature works from `landline-cli` before it gets UI.
- Small commits, one concern each. fmt, clippy, and tests pass before every commit.
- Do not push unless asked. The project will be open source at `tuhin-cmd/land-line` once the owner publishes it.
- Never commit real SMS content, phone numbers, device certificates, or keys. Scrub fixtures.
- Never log notification bodies, SMS text, clipboard contents, or OTPs above `trace` level.
- Check crate versions with `cargo add`; do not pin from memory.
