#!/usr/bin/env bash
# Builds landline-ffi for both Mac architectures, merges them into one static
# library, generates the Swift bindings, and assembles LandlineFFI.xcframework
# inside apple/LandlineKit.
#
#   scripts/build-xcframework.sh            release build (default)
#   scripts/build-xcframework.sh --debug    debug build
#
# The xcframework is assembled by hand rather than with
# `xcodebuild -create-xcframework`, so this script needs only the Rust
# toolchain and the Xcode command line tools.
set -euo pipefail

profile=release
case "${1:-}" in
  "" | --release) ;;
  --debug) profile=debug ;;
  *)
    echo "usage: $0 [--debug|--release]" >&2
    exit 2
    ;;
esac

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

targets=(aarch64-apple-darwin x86_64-apple-darwin)
lib=liblandline_ffi.a
module=landline_ffiFFI # the C module name UniFFI gives the generated header
kit=apple/LandlineKit
xcframework=$kit/LandlineFFI.xcframework
slice=macos-arm64_x86_64
generated=$kit/Sources/LandlineKit/Generated
target_dir=${CARGO_TARGET_DIR:-target}
work=$target_dir/xcframework

# Match the app's deployment target so the linker does not warn about objects
# built for a newer macOS.
export MACOSX_DEPLOYMENT_TARGET=14.0

cargo_flags=(--package landline-ffi --lib)
if [[ $profile == release ]]; then
  cargo_flags+=(--release)
fi

installed=$(rustup target list --installed)
for target in "${targets[@]}"; do
  if ! grep -qx "$target" <<<"$installed"; then
    rustup target add "$target"
  fi
  cargo build "${cargo_flags[@]}" --target "$target"
done

rm -rf "$work"
mkdir -p "$work/bindings"

lipo -create \
  "$target_dir/aarch64-apple-darwin/$profile/$lib" \
  "$target_dir/x86_64-apple-darwin/$profile/$lib" \
  -output "$work/$lib"

# Bindings are read from the compiled library's metadata, so they cannot drift
# from the code that was just built.
cargo run --quiet --package landline-ffi --features bindgen --bin uniffi-bindgen -- \
  generate \
  --library "$target_dir/aarch64-apple-darwin/$profile/$lib" \
  --language swift \
  --out-dir "$work/bindings"

rm -rf "$xcframework" "$generated"
mkdir -p "$xcframework/$slice/Headers/$module" "$generated"

cp "$work/$lib" "$xcframework/$slice/$lib"
# The header and module map sit in a directory named after the module, which
# is where clang looks for it, and which keeps a second xcframework's
# module.modulemap from colliding with this one.
cp "$work/bindings/$module.h" "$xcframework/$slice/Headers/$module/"
cp "$work/bindings/$module.modulemap" "$xcframework/$slice/Headers/$module/module.modulemap"
cp "$work/bindings/landline_ffi.swift" "$generated/"

cat >"$xcframework/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AvailableLibraries</key>
	<array>
		<dict>
			<key>BinaryPath</key>
			<string>$lib</string>
			<key>HeadersPath</key>
			<string>Headers</string>
			<key>LibraryIdentifier</key>
			<string>$slice</string>
			<key>LibraryPath</key>
			<string>$lib</string>
			<key>SupportedArchitectures</key>
			<array>
				<string>arm64</string>
				<string>x86_64</string>
			</array>
			<key>SupportedPlatform</key>
			<string>macos</string>
		</dict>
	</array>
	<key>CFBundlePackageType</key>
	<string>XFWK</string>
	<key>XCFrameworkFormatVersion</key>
	<string>1.0</string>
</dict>
</plist>
EOF

echo "Built $xcframework ($profile: $(lipo -archs "$xcframework/$slice/$lib"))"
echo "Swift bindings in $generated"
