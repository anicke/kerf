#!/bin/sh
# Builds Kerf-<version>-x86_64.AppImage from release binaries.
#   packaging/linux/build-appimage.sh <folder with kerf, kerf-cli, kerf-mcp> <output file>
# APPIMAGETOOL and APPIMAGE_RUNTIME name appimagetool and the type2 runtime it embeds (the
# release workflow downloads pinned, checksummed copies of both).
set -eu
bins="$1"
out="$2"
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
id=io.github.anicke.Kerf

appdir="$(mktemp -d)/Kerf.AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/metainfo" \
    "$appdir/usr/share/mime/packages" "$appdir/usr/share/doc/kerf"
for bin in kerf kerf-cli kerf-mcp; do
    install -m 755 "$bins/$bin" "$appdir/usr/bin/"
done
install -m 755 "$here/AppRun" "$appdir/AppRun"
install -m 644 "$here/$id.desktop" "$appdir/usr/share/applications/"
install -m 644 "$here/$id.metainfo.xml" "$appdir/usr/share/metainfo/"
install -m 644 "$here/$id.xml" "$appdir/usr/share/mime/packages/"
for size in 16 24 32 48 64 128 256 512; do
    dir="$appdir/usr/share/icons/hicolor/${size}x$size/apps"
    mkdir -p "$dir"
    install -m 644 "$root/packaging/icons/kerf-$size.png" "$dir/$id.png"
done
mkdir -p "$appdir/usr/share/icons/hicolor/scalable/apps"
install -m 644 "$root/packaging/icons/kerf.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg"
install -m 644 "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$appdir/usr/share/doc/kerf/"
# appimagetool wants the desktop file and icon at the top as well.
ln -s "usr/share/applications/$id.desktop" "$appdir/$id.desktop"
ln -s "usr/share/icons/hicolor/256x256/apps/$id.png" "$appdir/$id.png"
ln -s "$id.png" "$appdir/.DirIcon"

# Extract-and-run: no FUSE needed on the build machine.
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream \
    --runtime-file "$APPIMAGE_RUNTIME" "$appdir" "$out"
rm -r "$(dirname "$appdir")"
