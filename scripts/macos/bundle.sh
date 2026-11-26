#!/bin/bash
# Builds Kibotos for macOS and packages it:
#   build/Kibotos.app            universal (Apple silicon + Intel), ad-hoc signed
#   build/kibotos-mac-vX.Y.dmg   the app and a shortcut to Applications
#
# Run on a Mac from the repository root:  scripts/macos/bundle.sh
# Needs the Rust targets aarch64-apple-darwin and x86_64-apple-darwin
# (rustup target add ...) and the Xcode command line tools.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$(pwd)
CRATE=crates/kibotos-mac
OUT=$ROOT/build
export MACOSX_DEPLOYMENT_TARGET=11.0

# Version from the crate: 0.1.0 -> "0.1", 0.1.2 -> "0.1.2".
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -n 1)
SHORT=$VERSION
if [[ $VERSION =~ ^([0-9]+\.[0-9]+)\.0$ ]]; then
    SHORT=${BASH_REMATCH[1]}
fi
echo "Kibotos for macOS $VERSION"

# 1. Universal binary.
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked -p kibotos-mac --target "$target"
done
rm -rf "$OUT"
mkdir -p "$OUT"
lipo -create \
    target/aarch64-apple-darwin/release/kibotos-mac \
    target/x86_64-apple-darwin/release/kibotos-mac \
    -output "$OUT/Kibotos"
lipo -info "$OUT/Kibotos"

# 2. The .app bundle.
APP=$OUT/Kibotos.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
mv "$OUT/Kibotos" "$APP/Contents/MacOS/Kibotos"
sed "s/@VERSION@/$VERSION/g" "$CRATE/macos/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp licenses/unRAR-license.txt "$APP/Contents/Resources/"

# 3. The icon, from the square 1024 px PNG.
ICONSET=$OUT/kibotos.iconset
mkdir -p "$ICONSET"
SRC=$CRATE/assets/icon-1024.png
for size in 16 32 128 256 512; do
    sips -z $size $size "$SRC" --out "$ICONSET/icon_${size}x${size}.png" > /dev/null
    double=$((size * 2))
    sips -z $double $double "$SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/kibotos.icns"
rm -rf "$ICONSET"

# 4. Ad-hoc signature (no Apple developer account): Gatekeeper still asks
#    once, but the app runs on Apple silicon, which refuses unsigned code.
codesign --force --deep -s - "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# 5. The disk image, with a shortcut to Applications to drag the app onto.
STAGE=$OUT/dmg
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
DMG=$OUT/kibotos-mac-v$SHORT.dmg
hdiutil create -volname "Kibotos" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo "Done: $DMG"
