#!/usr/bin/env bash
# Mounts the release .dmg, installs the .app and proves it runs.
#
#   smoke/macos.sh <path-to.dmg> <expected-version>
#
# The macOS counterpart of linux.sh and windows.ps1. The app has no console, so
# the version comes from the bundle's Info.plist and "does it start" is the exit
# code of `<app> --smoke`, which returns before any window is created.
set -euo pipefail

dmg="${1:?usage: macos.sh <dmg> <version>}"
want="${2:?usage: macos.sh <dmg> <version>}"

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

work="$(mktemp -d)"
mnt="$work/mnt"
mkdir "$mnt"
cleanup() {
  hdiutil detach "$mnt" -quiet -force 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

hdiutil attach "$dmg" -nobrowse -readonly -quiet -mountpoint "$mnt"
src="$(find "$mnt" -maxdepth 1 -name '*.app' -print -quit)"
[ -n "$src" ] || fail "the dmg contains no .app"

# Install the way a user does: drag the app out of the image.
app="$work/$(basename "$src")"
ditto "$src" "$app"

plist="$app/Contents/Info.plist"
have="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist")"
[ "$have" = "$want" ] || fail "installed app is $have, expected $want"

exe="$app/Contents/MacOS/$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist")"
[ -x "$exe" ] || fail "no executable at $exe"
echo "installed $(basename "$app") $have"

# Every library the binary links must be a system one or inside the bundle; a
# path into the build machine (e.g. /opt/homebrew) would not exist for a user.
libs="$(otool -L "$exe")"   # fails closed: a non-Mach-O binary aborts here
if bad="$(printf '%s\n' "$libs" | tail -n +2 | awk '{print $1}' |
    grep -Ev '^(/usr/lib/|/System/Library/|@rpath/|@executable_path/|@loader_path/)')"; then
  fail "the binary links outside the system and the bundle:
$bad"
fi

"$exe" --smoke || fail "'$exe --smoke' exited $?"
echo "SMOKE OK: $(basename "$app") $have"
