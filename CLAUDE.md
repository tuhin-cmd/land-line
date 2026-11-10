# Landline

macOS menu bar app that links to an Android phone through the stock KDE Connect app. `PLAN.md` holds the decisions, milestones, and risks. Read it first, and update it in the same commit when it and reality disagree.

## Commands

```sh
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo run -p landline-cli -- heartbeat   # drive the core headlessly

./scripts/build-app.sh --run             # Rust core + Swift app -> build/Landline.app, then relaunch it
./scripts/test-swift.sh                  # Swift tests; needs the xcframework from a build first
```

Both scripts build debug for this Mac's architecture. `--release --universal` is what CI builds.

There is no Xcode project and full Xcode is not installed: the Command Line Tools are enough, and the Mac is short on disk. Do not add a step that needs `xcodebuild`. Before a large build, check free space; `cargo clean` and `rm -rf apple/.build` get it back.

`apple/LandlineFFI.xcframework`, `apple/Sources/LandlineKit/Generated`, and `build/` are build outputs and are not committed.

## Conventions

- The core owns the protocol and network state and never calls a macOS API. Swift owns everything the OS provides. `landline-ffi` adapts types and holds no logic.
- Every feature works from `landline-cli` before it gets UI.
- Small commits, one concern each. fmt, clippy, and tests pass before every commit.
- Do not push unless asked. The project will be open source at `tuhin-cmd/land-line` once the owner publishes it.
- Never commit real SMS content, phone numbers, device certificates, or keys. Scrub fixtures.
- Never log notification bodies, SMS text, clipboard contents, or OTPs above `trace` level.
- Check crate versions with `cargo add`; do not pin from memory.
