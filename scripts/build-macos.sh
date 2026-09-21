#!/bin/sh
# Native Apple Silicon build. Run via: sh scripts/build-macos.sh
set -eu
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo 'Build on an Apple Silicon Mac (without Rosetta).' >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET=14.0
TARGET=aarch64-apple-darwin
rustup target add "$TARGET"
cargo build --locked --release --target "$TARGET"
APP="$PWD/dist/Zapret UI.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/zapret-ui" "$APP/Contents/MacOS/zapret-ui"
cp packaging/macos/Info.plist "$APP/Contents/Info.plist"
chmod 755 "$APP/Contents/MacOS/zapret-ui"
# Sign the complete bundle after all contents have been written.
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"
file "$APP/Contents/MacOS/zapret-ui"
ZIP="$PWD/dist/zapret-ui-macos-arm64.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
shasum -a 256 "$ZIP" > "$ZIP.sha256"
printf '\nBuilt: %s\nRun: open "%s"\n' "$ZIP" "$APP"
