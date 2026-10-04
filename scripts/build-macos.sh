#!/bin/sh
# Native Apple Silicon build and drag-to-Applications distribution.
# --package-only reuses an already built release binary.
set -eu
cd "$(dirname "$0")/.."
case "${1:-}" in
    '') BUILD=1 ;;
    --package-only) BUILD=0 ;;
    *) echo 'Usage: sh scripts/build-macos.sh [--package-only]' >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then
    echo 'Usage: sh scripts/build-macos.sh [--package-only]' >&2
    exit 2
fi
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo 'Build on an Apple Silicon Mac (without Rosetta).' >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET=14.0
TARGET=aarch64-apple-darwin
PACKAGE_VERSION=$(sed -nE 's/^version = "([^"]+)"$/\1/p' Cargo.toml | head -n 1)
VERSION=${ZAPRET_UI_VERSION:-$(git describe --tags --always --dirty 2>/dev/null || printf '%s' "$PACKAGE_VERSION")}
VERSION=${VERSION#v}
# Keep the binary's About page and bundle metadata on the same revision.
export ZAPRET_UI_VERSION="$VERSION"
BUNDLE_VERSION=$(printf '%s\n' "$VERSION" | sed -nE 's/^([0-9]+\.[0-9]+\.[0-9]+)([-+].*)?$/\1/p')
BUNDLE_VERSION=${BUNDLE_VERSION:-$PACKAGE_VERSION}

if [ "$BUILD" -eq 1 ]; then
    rustup target add "$TARGET"
    cargo build --locked --release --target "$TARGET"
fi
BINARY="$PWD/target/$TARGET/release/zapret-ui"
if [ ! -x "$BINARY" ] || [ "$(lipo -archs "$BINARY")" != arm64 ]; then
    echo "Expected a native arm64 release binary at $BINARY" >&2
    exit 1
fi

mkdir -p dist
STAGE=$(mktemp -d "$PWD/dist/.macos-package.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT HUP INT TERM
APP="$STAGE/Zapret UI.app"
RESOURCES="$APP/Contents/Resources"
mkdir -p "$APP/Contents/MacOS" "$RESOURCES"
cp "$BINARY" "$APP/Contents/MacOS/zapret-ui"
cp packaging/macos/Info.plist "$APP/Contents/Info.plist"
cp LICENSE NOTICE.md "$RESOURCES/"
chmod 755 "$APP/Contents/MacOS/zapret-ui"
plutil -replace CFBundleShortVersionString -string "$BUNDLE_VERSION" "$APP/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$BUNDLE_VERSION" "$APP/Contents/Info.plist"
plutil -insert ZapretUIVersion -string "$VERSION" "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

# Build a Retina icon from the existing application artwork using macOS tools.
ICONSET="$STAGE/AppIcon.iconset"
mkdir -p "$ICONSET"
for SIZE in 16 32 128 256 512; do
    sips -z "$SIZE" "$SIZE" assets/icon-1024.png --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
    DOUBLE=$((SIZE * 2))
    sips -z "$DOUBLE" "$DOUBLE" assets/icon-1024.png --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$RESOURCES/AppIcon.icns"

# The default is a local ad-hoc signature. A maintainer can supply a Developer
# ID identity from their keychain; notarization remains a separate release step.
IDENTITY=${ZAPRET_UI_SIGNING_IDENTITY:--}
if [ "$IDENTITY" = - ]; then
    codesign --force --sign - "$APP"
else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
fi
codesign --verify --deep --strict "$APP"
file "$APP/Contents/MacOS/zapret-ui"

ZIP=zapret-ui-macos-arm64.zip
DMG=zapret-ui-macos-arm64.dmg
ditto -c -k --sequesterRsrc --keepParent "$APP" "$STAGE/$ZIP"
mkdir "$STAGE/dmg"
ditto "$APP" "$STAGE/dmg/Zapret UI.app"
ln -s /Applications "$STAGE/dmg/Applications"
hdiutil create -quiet -volname 'Zapret UI' -srcfolder "$STAGE/dmg" -format UDZO "$STAGE/$DMG"
hdiutil verify -quiet "$STAGE/$DMG"
# Relative filenames let users verify these files after downloading anywhere.
(cd "$STAGE" && shasum -a 256 "$ZIP" > "$ZIP.sha256" && shasum -a 256 "$DMG" > "$DMG.sha256")

# Promote only complete, verified outputs, with no stale files in the app.
rm -rf "$PWD/dist/Zapret UI.app"
mv "$APP" "$PWD/dist/Zapret UI.app"
for OUTPUT in "$ZIP" "$ZIP.sha256" "$DMG" "$DMG.sha256"; do
    mv -f "$STAGE/$OUTPUT" "$PWD/dist/$OUTPUT"
done
printf '\nBuilt: %s/dist/%s\nBuilt: %s/dist/%s\nRun: open "%s/dist/Zapret UI.app"\n' "$PWD" "$ZIP" "$PWD" "$DMG" "$PWD"
