#!/usr/bin/env bash
# Builds landline-ffi as a static library, generates the Swift bindings, and
# assembles apple/LandlineFFI.xcframework.
#
#   scripts/build-xcframework.sh [--release] [--universal]
#
# Builds a debug library for this Mac's architecture unless told otherwise.
# --universal builds arm64 and x86_64 and merges them.
#
# The xcframework is assembled by hand rather than with
# `xcodebuild -create-xcframework`, so this script needs only the Rust
# toolchain and the Xcode command line tools.
set -euo pipefail

profile=debug
universal=false
for arg in "$@"; do
  case "$arg" in
    --debug) profile=debug ;;
    --release) profile=release ;;
    --universal) universal=true ;;
    *)
      echo "usage: $0 [--release] [--universal]" >&2
      exit 2
      ;;
  esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

if $universal; then
  archs=(arm64 x86_64)
else
  archs=("$(uname -m)")
fi

rust_target() {
  case "$1" in
    arm64) echo aarch64-apple-darwin ;;
    x86_64) echo x86_64-apple-darwin ;;
    *)
      echo "unsupported architecture: $1" >&2
      exit 1
      ;;
  esac
}

lib=liblandline_ffi.a
module=landline_ffiFFI # the C module name UniFFI gives the generated header
xcframework=apple/LandlineFFI.xcframework
slice=macos-$(IFS=_ && echo "${archs[*]}")
generated=apple/Sources/LandlineKit/Generated
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
libs=()
for arch in "${archs[@]}"; do
  target=$(rust_target "$arch")
  if ! grep -qx "$target" <<<"$installed"; then
    rustup target add "$target"
  fi
  cargo build "${cargo_flags[@]}" --target "$target"
  libs+=("$target_dir/$target/$profile/$lib")
done

rm -rf "$work"
mkdir -p "$work/bindings"

# Bindings are read from the compiled library's metadata, so they cannot drift
# from the code that was just built.
cargo run --quiet --package landline-ffi --features bindgen --bin uniffi-bindgen -- \
  generate \
  --library "${libs[0]}" \
  --language swift \
  --out-dir "$work/bindings"

rm -rf "$xcframework" "$generated"
mkdir -p "$xcframework/$slice/Headers/$module" "$generated"

lipo -create "${libs[@]}" -output "$xcframework/$slice/$lib"
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
$(printf '\t\t\t\t<string>%s</string>\n' "${archs[@]}")
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

echo "Built $xcframework ($profile, ${archs[*]})"
