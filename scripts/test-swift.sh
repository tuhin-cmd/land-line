#!/usr/bin/env bash
# Runs the Swift tests. Run scripts/build-xcframework.sh (or build-app.sh)
# first. Extra arguments go to `swift test`.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

flags=()
developer_dir=$(xcode-select -p)
if [[ $developer_dir == */CommandLineTools ]]; then
  # Without full Xcode, SwiftPM does not find the Swift Testing framework or
  # its macro plugin by itself, and the tools ship no module for the
  # Testing/Foundation overlay, so that overlay is switched off.
  frameworks=$developer_dir/Library/Developer/Frameworks
  flags=(
    -Xswiftc -F"$frameworks"
    -Xswiftc -plugin-path -Xswiftc "$developer_dir/usr/lib/swift/host/plugins/testing"
    -Xswiftc -Xfrontend -Xswiftc -disable-cross-import-overlays
    -Xlinker -F"$frameworks"
    -Xlinker -rpath -Xlinker "$frameworks"
  )
fi

# ${flags[@]+...} keeps bash 3.2 from treating an empty array as unset.
swift test --package-path "$root/apple" ${flags[@]+"${flags[@]}"} "$@"
