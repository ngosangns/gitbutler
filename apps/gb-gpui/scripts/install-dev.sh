#!/usr/bin/env bash
# Build this workspace with devtools and install "~/Applications/gb-gpui Dev.app".
# The release app is not touched. A running Dev copy is quit and reopened on
# the same GitButler project list as GitButler.app. CHANNEL=release selects
# that data directory, and the reopen does not set GB_GPUI_FAKE.
#
# usage: install-dev.sh [--force] [--quiet]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APPS_DIR="${NS_DEV_APPS_DIR:-$HOME/Applications}"
NAME="gb-gpui"
DISPLAY="gb-gpui Dev"
BUNDLE_ID="com.ngosangns.gb-gpui.dev"
BUNDLE="$APPS_DIR/$DISPLAY.app"
ICON="$APP_ROOT/assets/gb-gpui.png"
EXE="$NAME"
BIN="${CARGO_TARGET_DIR:-$APP_ROOT/target}/release/$EXE"

FORCE=0
QUIET=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --quiet) QUIET=1 ;;
    *) echo "usage: $0 [--force] [--quiet]" >&2; exit 2 ;;
  esac
done

say() { [ "$QUIET" = 1 ] || echo "$@"; }

quit_dev_app() {
  local proc_pat="$BUNDLE/Contents/MacOS/"
  local _
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -f "$proc_pat" >/dev/null 2>&1 || break
    sleep 0.3
  done
  pkill -f "$proc_pat" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -f "$proc_pat" >/dev/null 2>&1 || break
    sleep 0.3
  done
}

# The installed Dev.app is on canned data when its process has this variable.
dev_app_on_fake() {
  local proc_pat="$BUNDLE/Contents/MacOS/"
  local pid
  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    if ps eww -p "$pid" 2>/dev/null | grep -q 'GB_GPUI_FAKE='; then
      return 0
    fi
  done < <(pgrep -f "$proc_pat" || true)
  return 1
}

reopen_dev_app() {
  # -g keeps the current app in front. Omit -j so the window is on screen.
  open -g --env NS_DEV_QUIET=1 "$BUNDLE"
}

say "==> $DISPLAY: cargo build"
cargo_args=(build --release -p gb-gpui --features devtools)
if [ "$QUIET" = 1 ]; then
  (cd "$APP_ROOT" && cargo "${cargo_args[@]}" --quiet 2>&1 | grep -E "^error" -A8 || true; exit "${PIPESTATUS[0]}")
else
  (cd "$APP_ROOT" && cargo "${cargo_args[@]}")
fi

ASKPASS="$(dirname "$BIN")/gitbutler-git-askpass"
if [ ! -x "$ASKPASS" ]; then
  echo "askpass helper missing: $ASKPASS" >&2
  exit 1
fi
sum="$(shasum -a 256 "$BIN" "$ASKPASS" "$ICON" "$SCRIPT_DIR/dev-icon.py" | shasum -a 256 | cut -c1-16)"
if [ "$FORCE" = 0 ] && [ -f "$BUNDLE/Contents/Resources/dev-build" ] && [ "$(cat "$BUNDLE/Contents/Resources/dev-build")" = "$sum" ]; then
  say "==> $DISPLAY: up to date"
  if dev_app_on_fake; then
    say "==> $DISPLAY: reopening on the GitButler project list"
    quit_dev_app
    reopen_dev_app
  fi
  exit 0
fi

repo="$(git -C "$APP_ROOT" rev-parse --show-toplevel 2>/dev/null || true)"
base_version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$APP_ROOT/crates/gb-gpui/Cargo.toml" | head -1)"
[ -n "$base_version" ] || base_version=0.0.0
if [ -n "$repo" ]; then
  git_rev="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo nogit)"
  dirty=""
  [ -z "$(git -C "$repo" status --porcelain -- apps/gb-gpui 2>/dev/null)" ] || dirty=".dirty"
else
  git_rev="nogit"
  dirty=""
fi
version="$base_version-dev+$git_rev$dirty"

stage="$APPS_DIR/.$NAME-Dev.staging.$$.app"
rm -rf "$stage"
mkdir -p "$stage/Contents/MacOS" "$stage/Contents/Resources"
cp "$BIN" "$stage/Contents/MacOS/$EXE"
cp "$ASKPASS" "$stage/Contents/MacOS/gitbutler-git-askpass"
chmod 755 "$stage/Contents/MacOS/gitbutler-git-askpass"
python3 "$SCRIPT_DIR/dev-icon.py" "$ICON" "$stage/Contents/Resources/AppIcon.icns"
cat >"$stage/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key><string>$DISPLAY</string>
	<key>CFBundleName</key><string>$DISPLAY</string>
	<key>CFBundleExecutable</key><string>$EXE</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST
printf '%s' "$sum" >"$stage/Contents/Resources/dev-build"
codesign --force --sign - "$stage" >/dev/null 2>&1

proc_pat="$BUNDLE/Contents/MacOS/"
was_running=0
if pgrep -f "$proc_pat" >/dev/null 2>&1; then
  was_running=1
  quit_dev_app
fi
mkdir -p "$APPS_DIR"
rm -rf "$BUNDLE"
mv "$stage" "$BUNDLE"
touch "$BUNDLE"
say "==> $DISPLAY: installed $BUNDLE ($version)"
if [ "$was_running" = 1 ]; then
  reopen_dev_app
fi
