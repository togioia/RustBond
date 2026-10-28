#!/usr/bin/env bash
# Build del client macOS universale + bundle .app.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release -p rnb-client --target aarch64-apple-darwin
cargo build --release -p rnb-client --target x86_64-apple-darwin

mkdir -p out
lipo -create -output out/rustbond \
  target/aarch64-apple-darwin/release/rustbond \
  target/x86_64-apple-darwin/release/rustbond

APP="out/RustBond.app"
mkdir -p "$APP/Contents/MacOS"
cp out/rustbond "$APP/Contents/MacOS/RustBond"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>RustBond</string>
  <key>CFBundleIdentifier</key><string>dev.algios.rustbond</string>
  <key>CFBundleExecutable</key><string>RustBond</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$APP"
ditto -c -k --sequesterRsrc --keepParent "$APP" out/RustBond-macos-universal.zip

echo "OK: out/RustBond.app e out/RustBond-macos-universal.zip"
