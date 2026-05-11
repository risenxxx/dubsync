#!/usr/bin/env bash
# Builds Dubsync.app and dubsync-vX.Y.Z-<target>.dmg from already-compiled cargo
# binaries plus Homebrew-installed ffmpeg, ffprobe, rubberband. Bundles every
# dylib dependency next to the binaries via dylibbundler so the .app runs on
# machines without Homebrew. Ad-hoc signs everything so Apple Silicon kernel
# accepts the binaries; users still need to pass System Settings → Privacy &
# Security → Open Anyway on first launch (no Developer ID, no notarization).
#
# Usage:
#   installer/macos/build-app.sh --version 0.6.0
#   installer/macos/build-app.sh --version 0.6.0 --target aarch64-apple-darwin
#
# Run from the repository root after `cargo build --release --features gui
# --target <target>`.

set -euo pipefail

VERSION=""
TARGET="aarch64-apple-darwin"
TARGET_DIR="target"
OUT_DIR="target/installer"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)    VERSION="$2";    shift 2 ;;
        --target)     TARGET="$2";     shift 2 ;;
        --target-dir) TARGET_DIR="$2"; shift 2 ;;
        --out-dir)    OUT_DIR="$2";    shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$VERSION" ]]; then
    echo "error: --version is required" >&2
    exit 2
fi

if [[ "$(uname)" != "Darwin" ]]; then
    echo "error: must run on macOS (got $(uname))" >&2
    exit 1
fi

for cmd in dylibbundler codesign hdiutil otool install_name_tool; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "error: $cmd not found in PATH (try: brew install dylibbundler)" >&2
        exit 1
    fi
done

if ! command -v brew >/dev/null 2>&1; then
    echo "error: Homebrew not found — install from https://brew.sh first" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

CARGO_BIN_DIR="$TARGET_DIR/$TARGET/release"
for bin in dubsync dubsync-gui; do
    if [[ ! -x "$CARGO_BIN_DIR/$bin" ]]; then
        echo "error: $CARGO_BIN_DIR/$bin missing — run 'cargo build --release --features gui --target $TARGET' first" >&2
        exit 1
    fi
done

FFMPEG_PREFIX="$(brew --prefix ffmpeg)"
RUBBERBAND_PREFIX="$(brew --prefix rubberband)"
FFMPEG_BIN="$FFMPEG_PREFIX/bin/ffmpeg"
FFPROBE_BIN="$FFMPEG_PREFIX/bin/ffprobe"
RUBBERBAND_BIN="$RUBBERBAND_PREFIX/bin/rubberband"

for src in "$FFMPEG_BIN" "$FFPROBE_BIN" "$RUBBERBAND_BIN"; do
    if [[ ! -x "$src" ]]; then
        echo "error: $src missing — run 'brew install ffmpeg rubberband'" >&2
        exit 1
    fi
done

APP_DIR="$OUT_DIR/Dubsync.app"
APP_MACOS="$APP_DIR/Contents/MacOS"
APP_RES="$APP_DIR/Contents/Resources"

echo "==> Cleaning $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_MACOS" "$APP_RES"

echo "==> Copying cargo binaries"
cp "$CARGO_BIN_DIR/dubsync"     "$APP_MACOS/"
cp "$CARGO_BIN_DIR/dubsync-gui" "$APP_MACOS/"

echo "==> Copying bundled tools (ffmpeg, ffprobe, rubberband)"
cp "$FFMPEG_BIN"     "$APP_MACOS/ffmpeg"
cp "$FFPROBE_BIN"    "$APP_MACOS/ffprobe"
cp "$RUBBERBAND_BIN" "$APP_MACOS/rubberband"
chmod +w "$APP_MACOS"/{ffmpeg,ffprobe,rubberband}

echo "==> Rendering Info.plist"
sed "s/__VERSION__/$VERSION/g" \
    "$REPO_ROOT/installer/macos/Info.plist.template" \
    > "$APP_DIR/Contents/Info.plist"

# macOS icon master: prefer `logo-macos.png` (drawn with the ~10% transparent
# inset Apple's HIG requires so the squircle sits at the same visual size as
# native apps in the Dock) and fall back to the shared `logo.png` when no
# macOS-specific master exists. Windows uses a tighter squircle, so the two
# masters intentionally diverge.
if [[ -f assets/logo-macos.png ]]; then
    LOGO_SRC="assets/logo-macos.png"
elif [[ -f assets/logo.png ]]; then
    LOGO_SRC="assets/logo.png"
else
    LOGO_SRC=""
fi

if [[ -n "$LOGO_SRC" ]]; then
    echo "==> Generating .icns from $LOGO_SRC"
    # Apple's iconutil expects a .iconset directory with the canonical PNG
    # names. sips ships with macOS, so no extra installs are needed.
    ICONSET_PARENT="$(mktemp -d)"
    ICONSET_DIR="$ICONSET_PARENT/dubsync.iconset"
    mkdir -p "$ICONSET_DIR"
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$LOGO_SRC" \
            --out "$ICONSET_DIR/icon_${size}x${size}.png" >/dev/null
        retina=$((size * 2))
        sips -z "$retina" "$retina" "$LOGO_SRC" \
            --out "$ICONSET_DIR/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET_DIR" -o "$APP_RES/dubsync.icns"
    rm -rf "$ICONSET_PARENT"
else
    echo "==> Note: neither assets/logo-macos.png nor assets/logo.png present — using default app icon"
    # Drop CFBundleIconFile so Finder doesn't render a generic broken icon.
    /usr/libexec/PlistBuddy -c "Delete :CFBundleIconFile" "$APP_DIR/Contents/Info.plist" 2>/dev/null || true
fi

# License and README — copied to Resources for inspection inside the .app.
for f in FFMPEG-LICENSE.txt RUBBERBAND-LICENSE.txt README.md; do
    [[ -f "$f" ]] && cp "$f" "$APP_RES/"
done

echo "==> Bundling dylib dependencies via dylibbundler"
# -of: overwrite output; -b: copy libs; -cd: create destination dir.
# -x BIN: scan that binary's libs; multiple -x allowed, dependencies dedupe.
# -d / -p: where to put libs and how to name them in install_name records.
dylibbundler \
    -of -b -cd \
    -x "$APP_MACOS/ffmpeg" \
    -x "$APP_MACOS/ffprobe" \
    -x "$APP_MACOS/rubberband" \
    -d "$APP_MACOS/" \
    -p "@executable_path/"

echo "==> Verifying no Homebrew paths remain"
for bin in ffmpeg ffprobe rubberband; do
    leaks="$(otool -L "$APP_MACOS/$bin" \
        | awk 'NR>1 {print $1}' \
        | grep -E '^(/opt/homebrew|/usr/local)/' || true)"
    if [[ -n "$leaks" ]]; then
        echo "error: $bin still references absolute paths:" >&2
        echo "$leaks" >&2
        exit 1
    fi
done

echo "==> Ad-hoc signing inner binaries and dylibs"
# Sign inside-out: dylibs first, then executables, then the bundle. Apple
# deprecated --deep, so do it explicitly. Empty identity (-s -) is ad-hoc;
# this is enough for Apple Silicon code-signature checks. No notarization.
find "$APP_MACOS" -type f -name '*.dylib' -print0 \
    | xargs -0 -n1 codesign -s - --force --timestamp=none

for bin in ffmpeg ffprobe rubberband dubsync dubsync-gui; do
    codesign -s - --force --timestamp=none "$APP_MACOS/$bin"
done

echo "==> Ad-hoc signing the .app bundle"
codesign -s - --force --timestamp=none "$APP_DIR"

echo "==> Verifying signature"
codesign --verify --verbose=2 "$APP_DIR"

echo "==> Building .dmg"
mkdir -p "$OUT_DIR"
DMG_PATH="$OUT_DIR/dubsync-v${VERSION}-${TARGET}.dmg"
rm -f "$DMG_PATH"

# Stage with /Applications symlink for drag-to-install UX.
DMG_STAGE="$(mktemp -d)"
trap 'rm -rf "$DMG_STAGE"' EXIT
cp -R "$APP_DIR" "$DMG_STAGE/"
ln -s /Applications "$DMG_STAGE/Applications"

hdiutil create \
    -volname "dubsync $VERSION" \
    -srcfolder "$DMG_STAGE" \
    -ov -format UDZO \
    "$DMG_PATH" >/dev/null

echo
echo "Done."
echo "  App: $APP_DIR"
echo "  DMG: $DMG_PATH"
echo
echo "Note: unsigned distribution. On first launch users see Gatekeeper block,"
echo "must allow via System Settings → Privacy & Security → Open Anyway."
