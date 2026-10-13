#!/usr/bin/env bash
#
# Builds the macOS release on THIS Mac: one universal (Apple silicon + Intel) BHCapture.app, signed with
# the persistent "BHTerminal Dev" identity, wrapped as a branded .pkg and a .dmg.
#
# Why here and not on GitHub: the signing certificate IS the Screen Recording permission's identity
# (decision 4/6). Signing locally keeps its private key on this Mac instead of in a repository secret,
# and every update keeps users' permission. NEVER ship an ad-hoc build: each would reset that permission.
#
# Usage: scripts/release-macos.sh [out-dir]      (default: dist/)

set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-"$HERE/dist"}
IDENTITY="BHTerminal Dev"
BUNDLE_ID=com.biswashost.bhcapture

# rustup's toolchain has both macOS standard libraries; Homebrew's has only the host one.
TC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin"
export PATH="$TC/bin:$PATH" RUSTC="$TC/bin/rustc"
# Its own build folder: rustup's and Homebrew's compilers must not share proc-macro builds.
export CARGO_TARGET_DIR="$HERE/target.noindex/xmac"

cd "$HERE/app"
npx tauri build --target universal-apple-darwin --bundles app
APP="$CARGO_TARGET_DIR/universal-apple-darwin/release/bundle/macos/BHCapture.app"
[ -d "$APP" ] || { echo "no app bundle at $APP" >&2; exit 1; }

EXEC=$(/usr/libexec/PlistBuddy -c 'Print CFBundleExecutable' "$APP/Contents/Info.plist")
ARCHS=$(/usr/bin/lipo -archs "$APP/Contents/MacOS/$EXEC")
case " $ARCHS " in *" arm64 "*) ;; *) echo "not universal ($ARCHS)" >&2; exit 1 ;; esac
case " $ARCHS " in *" x86_64 "*) ;; *) echo "not universal ($ARCHS)" >&2; exit 1 ;; esac

codesign --force --deep --sign "$IDENTITY" --identifier "$BUNDLE_ID" --timestamp=none "$APP"
# The requirement must name the certificate, never a cdhash (an ad-hoc signature).
REQ=$(codesign -d -r- "$APP" 2>&1)
echo "$REQ" | grep -q 'certificate leaf' || { echo "signature does not use the certificate:"; echo "$REQ"; exit 1; } >&2
echo "$REQ" | grep -q 'cdhash' && { echo "ad-hoc signature (cdhash) -- refusing to ship" >&2; exit 1; }

VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")
echo "==> BHCapture $VERSION ($ARCHS), signed with $IDENTITY"
mkdir -p "$OUT"

# .pkg -- quits a running copy first and reopens it after (app/packaging/pkg-scripts).
PKGROOT=$(mktemp -d); PKGTMP=$(mktemp -d)
trap 'rm -rf "$PKGROOT" "$PKGTMP"' EXIT
mkdir -p "$PKGROOT/Applications" "$PKGTMP/res"
cp -R "$APP" "$PKGROOT/Applications/"
sed "s/__VERSION__/$VERSION/g" "$HERE/app/packaging/welcome.html"    > "$PKGTMP/res/welcome.html"
sed "s/__VERSION__/$VERSION/g" "$HERE/app/packaging/conclusion.html" > "$PKGTMP/res/conclusion.html"
pkgbuild --root "$PKGROOT" --install-location / --identifier "$BUNDLE_ID" --version "$VERSION" \
  --scripts "$HERE/app/packaging/pkg-scripts" --ownership recommended "$PKGTMP/component.pkg"
cat > "$PKGTMP/distribution.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="1">
  <title>BHCapture</title>
  <welcome file="welcome.html" mime-type="text/html"/>
  <conclusion file="conclusion.html" mime-type="text/html"/>
  <volume-check><allowed-os-versions><os-version min="14.0"/></allowed-os-versions></volume-check>
  <options customize="never" require-scripts="false" hostArchitectures="arm64,x86_64"/>
  <choices-outline><line choice="default"/></choices-outline>
  <choice id="default"><pkg-ref id="$BUNDLE_ID"/></choice>
  <pkg-ref id="$BUNDLE_ID" version="$VERSION">component.pkg</pkg-ref>
</installer-gui-script>
XML
PKG="$OUT/BHCapture-$VERSION-universal.pkg"
rm -f "$PKG"
productbuild --distribution "$PKGTMP/distribution.xml" --resources "$PKGTMP/res" --package-path "$PKGTMP" "$PKG"

# .dmg -- the app and an Applications shortcut to drag it onto.
DMGSRC=$(mktemp -d)
cp -R "$APP" "$DMGSRC/"
ln -s /Applications "$DMGSRC/Applications"
DMG="$OUT/BHCapture-$VERSION-universal.dmg"
rm -f "$DMG"
hdiutil create -quiet -volname "BHCapture $VERSION" -srcfolder "$DMGSRC" -fs HFS+ -format UDZO "$DMG"
rm -rf "$DMGSRC"

ls -lh "$PKG" "$DMG"
