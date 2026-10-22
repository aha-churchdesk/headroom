#!/bin/bash
# Assemble Headroom.app: the menu bar app (headroomd) with the CLI beside it.
#
# The CLI sits in Contents/MacOS so the app can find the `headroom` its
# agents' hooks call, and so one copy of the app is a complete install.
#
#   ./scripts/bundle.sh             build Headroom.app here
#   ./scripts/bundle.sh --install   and copy it to /Applications
set -euo pipefail

cd "$(dirname "$0")/.."
APP="Headroom.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

command -v mise >/dev/null 2>&1 && RUN="mise exec --" || RUN=""
$RUN cargo build --release --locked -p headroom -p headroomd

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/headroomd target/release/headroom "$APP/Contents/MacOS/"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Headroom</string>
  <key>CFBundleDisplayName</key>       <string>Headroom</string>
  <key>CFBundleIdentifier</key>        <string>dev.headroom</string>
  <key>CFBundleVersion</key>           <string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleExecutable</key>        <string>headroomd</string>
  <key>LSMinimumSystemVersion</key>    <string>13.0</string>
  <key>LSArchitecturePriority</key>    <array><string>arm64</string></array>

  <!-- Menu bar only: no Dock icon, no app-switcher entry. -->
  <key>LSUIElement</key>               <true/>
</dict>
</plist>
PLIST

# Ad-hoc signature, inside out: the CLI first, then the bundle that seals it.
codesign --force --sign - "$APP/Contents/MacOS/headroom"
codesign --force --sign - "$APP"

echo "built $APP $VERSION  ($(du -sh "$APP" | cut -f1))"

if [ "${1:-}" = "--install" ]; then
  ./scripts/install-bundle.sh "$APP"
else
  echo "run with:  open $APP"
fi
echo "stop with: Quit in the popover's Settings"
