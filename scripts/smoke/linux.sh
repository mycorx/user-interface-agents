#!/usr/bin/env bash
# Installs the release .deb on a clean machine and proves it runs.
#
#   smoke/linux.sh <path-to.deb> <expected-version>
#
# Meant to run as root inside a bare ubuntu image (see release.yaml), so
# nothing the build job installed can mask a missing dependency. That is the
# point: `apt-get install ./pkg.deb` resolves the .deb's declared Depends
# against a machine that has none of them, and `ldd` then proves the binary's
# libraries really arrived.
set -euo pipefail

deb="${1:?usage: linux.sh <deb> <version>}"
want="${2:?usage: linux.sh <deb> <version>}"

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq ./"$deb" >/dev/null

pkg="$(dpkg-deb -f "$deb" Package)"
have="$(dpkg-query -W -f='${Version}' "$pkg")"
# A .deb version may carry a revision suffix (0.1.2-1); the app version is the
# part before it.
[ "${have%%-*}" = "$want" ] || fail "installed $pkg is $have, expected $want"

bin="$(dpkg -L "$pkg" | grep -E '^/usr/bin/[^/]+$' | head -n 1)"
[ -n "$bin" ] || fail "$pkg installed no binary under /usr/bin"
echo "installed $pkg $have, binary $bin"

if missing="$(ldd "$bin" | grep 'not found')"; then
  fail "the binary has unresolved libraries:
$missing"
fi

# Installed only now, after the ldd check, so xvfb's own X/GL libraries cannot
# mask a Depends the .deb forgot to declare.
apt-get install -y -qq xvfb xauth >/dev/null

# No display exists on the runner; xvfb gives GTK one. --smoke exits before
# any window is made, so this only proves the process starts and links.
xvfb-run -a "$bin" --smoke || fail "'$bin --smoke' exited $?"

apt-get remove -y -qq "$pkg" >/dev/null
[ ! -e "$bin" ] || fail "$bin is still there after uninstall"
echo "SMOKE OK: $pkg $have"
