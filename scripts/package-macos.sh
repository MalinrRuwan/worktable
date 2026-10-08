#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
APP_NAME=${APP_NAME:-Worktable}
BUNDLE_DIR=${BUNDLE_DIR:-"$ROOT_DIR/dist/$APP_NAME.app"}
BUNDLE_ID=${BUNDLE_ID:-com.worktable.app}
VERSION=${VERSION:-0.1.0}
DMG_PATH=${DMG_PATH:-"$ROOT_DIR/dist/$APP_NAME-$VERSION.dmg"}

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script packages a macOS app and must run on macOS." >&2
  exit 1
fi

cd "$ROOT_DIR"

# The AI agent is compiled into the binary through rig (see
# crates/worktable-ai). There is no JS worker, no Node, and no separate sidecar
# to bundle — one native binary is all that ships.
cargo build --release -p worktable-app

rm -rf "$BUNDLE_DIR"
mkdir -p \
  "$BUNDLE_DIR/Contents/MacOS" \
  "$BUNDLE_DIR/Contents/Resources/themes"

cp "$ROOT_DIR/target/release/worktable-app" "$BUNDLE_DIR/Contents/MacOS/$APP_NAME"
cp -R "$ROOT_DIR/themes/." "$BUNDLE_DIR/Contents/Resources/themes/"

# App icon: rasterise the source PNG into the .icns the bundle advertises.
# The artwork already sits on Apple's 824/1024 grid, so every size scales it
# with the standard mac inset.
ICONSET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$ICONSET"
ICON_SRC="$ROOT_DIR/crates/worktable-app/assets/app_icon.png"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  twice=$((size * 2))
  sips -z "$twice" "$twice" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$BUNDLE_DIR/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

cat > "$BUNDLE_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleExecutable</key>
  <string>$APP_NAME</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
</dict>
</plist>
PLIST

chmod 755 "$BUNDLE_DIR/Contents/MacOS/$APP_NAME"

CODESIGN_IDENTITY=${CODESIGN_IDENTITY:--}
strip -S -x "$BUNDLE_DIR/Contents/MacOS/$APP_NAME" 2>/dev/null || true
codesign --force --deep --sign "$CODESIGN_IDENTITY" "$BUNDLE_DIR"

# Drag-to-Applications disk image.
STAGING=$(mktemp -d)
cp -R "$BUNDLE_DIR" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
rm -f "$DMG_PATH" "$DMG_PATH.sha256"
hdiutil create \
  -volname "$APP_NAME $VERSION" \
  -srcfolder "$STAGING" \
  -ov -format UDZO \
  "$DMG_PATH"
rm -rf "$STAGING"
(
  cd "$(dirname "$DMG_PATH")"
  shasum -a 256 "$(basename "$DMG_PATH")"
) | tee "$DMG_PATH.sha256"

echo "Created $BUNDLE_DIR"
echo "Created $DMG_PATH"
du -sh "$BUNDLE_DIR" "$BUNDLE_DIR/Contents/MacOS/$APP_NAME" "$DMG_PATH"
