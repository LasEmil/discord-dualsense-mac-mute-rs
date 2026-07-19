#!/bin/bash
# Builds DiscordMute.app: the Swift menu bar front end plus the Rust server it
# supervises, assembled into a bundle by hand (no Xcode required).
set -euo pipefail

MACOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$MACOS_DIR")"
BUILD_DIR="$MACOS_DIR/build"
APP="$BUILD_DIR/DiscordMute.app"

echo "==> Building the Rust server"
cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"

echo "==> Building the Swift app"
swift build -c release --package-path "$MACOS_DIR"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$MACOS_DIR/.build/release/DiscordMute" "$APP/Contents/MacOS/DiscordMute"
cp "$REPO_DIR/target/release/discord-mute-rs" "$APP/Contents/MacOS/discord-mute-rs"

# The server serves the web UI from ./static relative to its working directory,
# which the app sets to Resources.
if [ -d "$REPO_DIR/static" ]; then
  cp -R "$REPO_DIR/static" "$APP/Contents/Resources/static"
fi

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DiscordMute</string>
  <key>CFBundleDisplayName</key><string>Discord Mute</string>
  <key>CFBundleIdentifier</key><string>com.emil.discordmute</string>
  <key>CFBundleExecutable</key><string>DiscordMute</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <!-- Menu bar only: no Dock icon, no application menu. -->
  <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" > /dev/null

echo "==> Signing (ad-hoc)"
# Sign the nested binary before the bundle: the outer signature seals the
# contents, so signing it first would invalidate the seal.
codesign --force --sign - "$APP/Contents/MacOS/discord-mute-rs"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"

echo
echo "Built $APP"
echo "Open it with:  open '$APP'"
echo
echo "Note: ad-hoc signatures change on every rebuild, so macOS may ask again"
echo "for Input Monitoring, and any launch-at-login registration may lapse."
