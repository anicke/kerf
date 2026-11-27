#!/bin/sh
# Builds a disk image holding Kerf.app (with kerf-cli and kerf-mcp inside) and a link to
# /Applications, from release binaries. Runs on macOS.
#   packaging/macos/build-dmg.sh <folder with kerf, kerf-cli, kerf-mcp> <version> <output .dmg>
# The app is signed ad hoc only (not notarised), so Gatekeeper asks on first start.
set -eu
bins="$1"
version="$2"
out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."

stage="$(mktemp -d)"
app="$stage/Kerf.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for bin in kerf kerf-cli kerf-mcp; do
    install -m 755 "$bins/$bin" "$app/Contents/MacOS/"
done
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
cp "$root/packaging/icons/kerf.icns" "$app/Contents/Resources/"
cp "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$app/Contents/Resources/"
# Inner programs first, then the bundle, which seals Info.plist and the resources.
codesign --force --sign - "$app/Contents/MacOS/kerf-cli" "$app/Contents/MacOS/kerf-mcp"
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"
ln -s /Applications "$stage/Applications"

hdiutil create -volname "Kerf $version" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$out"
rm -rf "$stage"
