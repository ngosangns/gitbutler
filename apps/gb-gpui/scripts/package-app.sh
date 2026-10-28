#!/usr/bin/env bash
# Package gb-gpui as a macOS .app under apps/gb-gpui/target/bundle/.
# Does not install, launch, sign, or write to ~/Applications.
#
# usage:
#   package-app.sh                 release build, no devtools, then --check
#   package-app.sh --debug         debug build, no devtools, then --check
#   package-app.sh --binary PATH   wrap an existing binary, then --check
#   package-app.sh --check         verify target/bundle/gb-gpui.app only
set -euo pipefail

resolve_dir() {
  local source="$1"
  local dir link
  while [ -L "$source" ]; do
    dir="$(cd -P "$(dirname "$source")" && pwd)"
    link="$(readlink "$source")"
    case "$link" in
      /*) source="$link" ;;
      *) source="$dir/$link" ;;
    esac
  done
  cd -P "$(dirname "$source")" && pwd
}

SCRIPT_DIR="$(resolve_dir "${BASH_SOURCE[0]}")"
APP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NAME="gb-gpui"
BUNDLE_ID="com.ngosangns.gb-gpui"
VERSION="0.22.3"
OUT="$APP_ROOT/target/bundle/${NAME}.app"
MARKERS=("ns-dev-" "NS_DEVTOOLS" "devtools: cannot listen")

CHECK_ONLY=0
DEBUG=0
BINARY=""

usage() {
  cat <<'EOF'
usage: package-app.sh [--check] [--debug] [--binary PATH]

Writes apps/gb-gpui/target/bundle/gb-gpui.app.
The build is release unless --debug is set. Neither build enables devtools.
--check verifies the bundle and does not build.
This script does not install, launch, or sign.
EOF
}

die() {
  printf 'package-app: %s\n' "$1" >&2
  exit "${2:-1}"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --debug) DEBUG=1; shift ;;
    --binary)
      shift
      [ "$#" -ge 1 ] || die "--binary needs a path" 2
      BINARY="$1"
      shift
      ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown argument: $1" 2 ;;
  esac
done

if [ "$CHECK_ONLY" -eq 1 ] && [ "$DEBUG" -eq 1 ]; then
  die "--check does not build; drop --debug" 2
fi
if [ "$CHECK_ONLY" -eq 1 ] && [ -n "$BINARY" ]; then
  die "--check does not build; drop --binary" 2
fi
if [ "$DEBUG" -eq 1 ] && [ -n "$BINARY" ]; then
  die "pass either --debug or --binary" 2
fi

case "$OUT" in
  "$HOME/Applications"|"$HOME/Applications"/*)
    die "refusing to write under ~/Applications" 2
    ;;
esac

plist_value() {
  plutil -extract "$1" raw -o - "$OUT/Contents/Info.plist"
}

check_bundle() {
  [ -d "$OUT" ] || die "bundle is missing: $OUT" 2
  [ -f "$OUT/Contents/Info.plist" ] || die "Info.plist is missing" 1
  local exe="$OUT/Contents/MacOS/$NAME"
  [ -f "$exe" ] || die "executable is missing: $exe" 1
  [ -x "$exe" ] || die "executable is not executable: $exe" 1
  [ -s "$OUT/Contents/Resources/AppIcon.icns" ] || die "AppIcon.icns is missing or empty" 1
  local helper="$OUT/Contents/MacOS/gitbutler-git-askpass"
  [ -x "$helper" ] || die "askpass helper is missing or not executable: $helper" 1
  local askpass="$OUT/Contents/Resources/ASKPASS.txt"
  [ -s "$askpass" ] || die "ASKPASS.txt is missing or empty" 1
  grep -q "gitbutler-git-askpass" "$askpass" || die "ASKPASS.txt does not name gitbutler-git-askpass" 1
  grep -q "GITBUTLER_ASKPASS_BIN" "$askpass" || die "ASKPASS.txt does not name GITBUTLER_ASKPASS_BIN" 1

  local key expected actual
  while IFS='=' read -r key expected; do
    actual="$(plist_value "$key")"
    [ "$actual" = "$expected" ] || die "$key is '$actual', expected '$expected'" 1
  done <<EOF
CFBundleIdentifier=$BUNDLE_ID
CFBundleName=$NAME
CFBundleDisplayName=$NAME
CFBundleExecutable=$NAME
CFBundleIconFile=AppIcon
CFBundlePackageType=APPL
CFBundleShortVersionString=$VERSION
CFBundleVersion=$VERSION
EOF

  local marker
  local hits=""
  for marker in "${MARKERS[@]}"; do
    if strings -a "$exe" | grep -F -q -- "$marker"; then
      hits="${hits}${marker}"$'\n'
    fi
  done
  if [ -n "$hits" ]; then
    printf 'package-app: devtools markers in %s:\n%s' "$exe" "$hits" >&2
    exit 1
  fi
  printf 'package-app: check ok %s\n' "$OUT"
}

if [ "$CHECK_ONLY" -eq 1 ]; then
  check_bundle
  exit 0
fi

if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "binary not found: $BINARY" 2
  SRC_BIN="$(cd -P "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"
else
  profile="release"
  cargo_args=(build --release -p gb-gpui -j 4)
  if [ "$DEBUG" -eq 1 ]; then
    profile="debug"
    cargo_args=(build -p gb-gpui -j 4)
  fi
  (cd "$APP_ROOT" && cargo "${cargo_args[@]}")
  SRC_BIN="$APP_ROOT/target/$profile/$NAME"
  [ -f "$SRC_BIN" ] || die "cargo did not write $SRC_BIN" 1
fi

ICON_SRC="$APP_ROOT/assets/gb-gpui.png"
ICON_PY="$SCRIPT_DIR/dev-icon.py"
[ -f "$ICON_SRC" ] || die "icon source missing: $ICON_SRC" 1
[ -f "$ICON_PY" ] || die "icon script missing: $ICON_PY" 1

STAGE="$APP_ROOT/target/bundle/.staging.$$.app"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
cp "$SRC_BIN" "$STAGE/Contents/MacOS/$NAME"
chmod 755 "$STAGE/Contents/MacOS/$NAME"
ASKPASS_SRC="$(dirname "$SRC_BIN")/gitbutler-git-askpass"
[ -x "$ASKPASS_SRC" ] || die "askpass helper missing beside the app binary: $ASKPASS_SRC" 1
cp "$ASKPASS_SRC" "$STAGE/Contents/MacOS/gitbutler-git-askpass"
chmod 755 "$STAGE/Contents/MacOS/gitbutler-git-askpass"
python3 "$ICON_PY" "$ICON_SRC" "$STAGE/Contents/Resources/AppIcon.icns"
cat >"$STAGE/Contents/Resources/ASKPASS.txt" <<'EOF'
gb-gpui ships gitbutler-git-askpass beside the app executable.

Git runs that helper when a fetch or push asks for a password. The helper
talks to this app, and the password is typed into the app dialog.

GITBUTLER_ASKPASS_BIN overrides that path when it already points at a file.
EOF
cat >"$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key><string>${NAME}</string>
	<key>CFBundleName</key><string>${NAME}</string>
	<key>CFBundleExecutable</key><string>${NAME}</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>${VERSION}</string>
	<key>CFBundleVersion</key><string>${VERSION}</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

rm -rf "$OUT"
mv "$STAGE" "$OUT"
check_bundle
