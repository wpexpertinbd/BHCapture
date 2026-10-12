#!/bin/bash
# Build + bundle + sign the 0a probe. One argument: the build marker.
# Signing with a CERTIFICATE (not ad-hoc) is the whole point -- it makes the designated
# requirement "identifier ... and certificate leaf = H..." instead of a per-build cdhash.
set -euo pipefail

MARKER="${1:?usage: make-app.sh <marker>}"
IDENTITY="BHTerminal Dev"
BUNDLE_ID="com.biswashost.bhcapture"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../../target.noindex/0a"
APP="$OUT/BHCapture.app"

mkdir -p "$OUT"
BHC_MARKER="$MARKER" cargo build --release --quiet

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$HERE/../../target.noindex/spike-0a/release/bhc-spike-0a" "$APP/Contents/MacOS/BHCapture"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>BHCapture</string>
  <key>CFBundleName</key><string>BHCapture</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>12.3</string>
</dict></plist>
PLIST

codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" --timestamp=none "$APP" 2>&1 | sed 's/^/  codesign: /'

echo "marker:  $MARKER"
echo -n "cdhash:  "; codesign -dvvv "$APP" 2>&1 | grep -i '^CDHash=' | cut -d= -f2
echo -n "DR:      "; codesign -d -r- "$APP" 2>&1 | grep -i 'designated' | sed 's/^# *//'
echo -n "auth:    "; codesign -dvvv "$APP" 2>&1 | grep -i '^Authority=' | head -1
