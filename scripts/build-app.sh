#!/usr/bin/env bash
# Builds build/Landline.app: the Rust core, the Swift app, and a signed
# bundle around them (ad-hoc unless LANDLINE_SIGN_IDENTITY is set). Needs the
# Rust toolchain and the Xcode command line tools; full Xcode is not required.
#
#   scripts/build-app.sh [--release] [--universal] [--run]
#
# Builds a debug app for this Mac's architecture unless told otherwise.
# --run relaunches the app after building.
set -euo pipefail

config=debug
universal=false
run=false
for arg in "$@"; do
  case "$arg" in
    --debug) config=debug ;;
    --release) config=release ;;
    --universal) universal=true ;;
    --run) run=true ;;
    *)
      echo "usage: $0 [--release] [--universal] [--run]" >&2
      exit 2
      ;;
  esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

core_flags=("--$config")
if $universal; then
  core_flags+=(--universal)
  archs=(arm64 x86_64)
else
  archs=("$(uname -m)")
fi
scripts/build-xcframework.sh "${core_flags[@]}"

binaries=()
for arch in "${archs[@]}"; do
  swift_flags=(--package-path apple --configuration "$config" --triple "$arch-apple-macosx14.0")
  swift build "${swift_flags[@]}" --product Landline
  binaries+=("$(swift build "${swift_flags[@]}" --show-bin-path)/Landline")
done

app=build/Landline.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
lipo -create "${binaries[@]}" -output "$app/Contents/MacOS/Landline"

# The app version is the core version, so there is one number to bump.
version=$(cargo pkgid --package landline-core | sed 's/.*[#@]//')
cp apple/Support/Info.plist "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"

# macOS ties permissions (Local Network, notifications) to the signature. An
# ad-hoc signature is a hash of this exact build, so every rebuild is a new
# app to macOS and has to be allowed again. Set LANDLINE_SIGN_IDENTITY to the
# name of a code signing certificate in your keychain to keep them: a
# self-signed one made in Keychain Access is enough for development, and a
# Developer ID is needed to give the app to anyone else.
codesign --force --sign "${LANDLINE_SIGN_IDENTITY:--}" --options runtime "$app"

echo "Built $app ($config, ${archs[*]}, version $version)"

if $run; then
  pkill -x Landline || true
  open "$app"
fi
