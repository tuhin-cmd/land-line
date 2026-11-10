# Landline

macOS menu bar app that links to an Android phone through the stock KDE Connect app. `PLAN.md` holds the decisions, milestones, and risks. Read it first, and update it in the same commit when it and reality disagree.

## Commands

```sh
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo run -p landline-cli -- heartbeat        # drive the core headlessly

./scripts/build-xcframework.sh                # Rust -> apple/LandlineKit (xcframework + Swift bindings); --debug for a faster build
cd apple/LandlineKit && swift test            # bridge tests (needs full Xcode, not just Command Line Tools)
cd apple && xcodegen generate && xcodebuild build -project Landline.xcodeproj -scheme Landline
```

Run the build script again after any change to `landline-ffi`'s exported surface. The xcframework, the generated Swift bindings, and `Landline.xcodeproj` are build outputs and are not committed.

## Conventions

- The core owns the protocol and network state and never calls a macOS API. Swift owns everything the OS provides. `landline-ffi` adapts types and holds no logic.
- Every feature works from `landline-cli` before it gets UI.
- Small commits, one concern each. fmt, clippy, and tests pass before every commit.
- Never commit real SMS content, phone numbers, device certificates, or keys. Scrub fixtures.
- Never log notification bodies, SMS text, clipboard contents, or OTPs above `trace` level.
- Check crate versions with `cargo add`; do not pin from memory.
