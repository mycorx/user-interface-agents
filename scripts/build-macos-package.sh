#!/usr/bin/env bash
# Builds a local macOS bundle (.app + .dmg) for the uia desktop app, running the
# same checks CI does first. macOS counterpart of build-windows-package.ps1.
#
# Nothing is installed on the system. Node, pnpm, Rust and Python all live in
# <repo>/.toolchain (gitignored), and the environment that points at them exists
# only inside this process. `rm -rf .toolchain` undoes everything. The one
# exception is Xcode Command Line Tools, which provide the linker and SDK and
# cannot be isolated; this script checks for them and says how to install them.
#
#   ./scripts/build-macos-package.sh                    # tests, then build (native arch)
#   ./scripts/build-macos-package.sh --skip-tests       # build only
#   ./scripts/build-macos-package.sh --target universal-apple-darwin
#   ./scripts/build-macos-package.sh --bootstrap-only   # install the toolchain, stop
#   ./scripts/build-macos-package.sh --with-notice      # also run gen-notice --check
#
# --with-notice is opt-in: `cargo metadata` downloads every crate in the
# lockfile for every platform (Windows ones included) to attribute them all.
#
# Output: target/[<triple>/]release/bundle/{macos/*.app,dmg/*.dmg}
#
# Signing is off unless the standard Tauri variables are set in the environment
# (APPLE_SIGNING_IDENTITY, APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID, or the API
# key trio). An unsigned build runs on this machine; one downloaded from
# elsewhere is quarantined by Gatekeeper (docs/RELEASE.md).

# Bash 4+ is needed by scripts/check-copyright-headers.sh (mapfile), and macOS
# ships 3.2. Re-exec under Homebrew's bash when this one is too old, so it works
# whether or not PATH was set up for it. Nothing below this guard is bash-4-only.
if [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
  for candidate in /opt/homebrew/bin/bash /usr/local/bin/bash; do
    if [ -x "$candidate" ]; then
      exec "$candidate" "$0" "$@"
    fi
  done
  echo "FAIL: bash ${BASH_VERSION} is too old (need 4+, for check-copyright-headers.sh)." >&2
  echo "      brew install bash   # installs beside /bin/bash; does not replace it" >&2
  exit 1
fi

set -euo pipefail

# ---- Pins: bump here ---------------------------------------------------------
NODE_VERSION="24.21.0"                       # CI uses Node 24
NODE_SHA256_ARM64="bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057"
NODE_SHA256_X64="1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097"
PYTHON_VERSION="3.14"                        # gen-notice needs tomllib (3.11+); scripts/release is 3.9 + stdlib only
RUST_CHANNEL="stable"                        # CI uses dtolnay/rust-toolchain@stable
# pnpm is not pinned here: Corepack reads `packageManager` from package.json.
# ------------------------------------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOL="$REPO_ROOT/.toolchain"

SKIP_TESTS=false
BOOTSTRAP_ONLY=false
WITH_NOTICE=false
TARGET=""

usage() { sed -n '2,23p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-tests) SKIP_TESTS=true ;;
    --bootstrap-only) BOOTSTRAP_ONLY=true ;;
    --with-notice) WITH_NOTICE=true ;;
    --target)
      [ $# -ge 2 ] || { echo "FAIL: --target needs a value" >&2; exit 2; }
      TARGET="$2"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "FAIL: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$TARGET" in
  ""|aarch64-apple-darwin|x86_64-apple-darwin|universal-apple-darwin) ;;
  *) echo "FAIL: --target must be aarch64-apple-darwin, x86_64-apple-darwin or universal-apple-darwin" >&2; exit 2 ;;
esac

step() { printf '\n\033[36m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die()  { printf '\033[31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "this script builds the macOS bundle and must run on macOS"

# Run from the repo root so every relative path in the CI scripts resolves.
cd "$REPO_ROOT"

# ---- Environment: this process only -------------------------------------------
export RUSTUP_HOME="$TOOL/rustup"
export CARGO_HOME="$TOOL/cargo"
export COREPACK_HOME="$TOOL/corepack"
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0
export UV_PYTHON_INSTALL_DIR="$TOOL/uv-python"
export UV_PYTHON_PREFERENCE=only-managed
export UV_CACHE_DIR="$TOOL/uv-cache"
export npm_config_store_dir="$TOOL/pnpm-store"   # pnpm's package store; default is under ~/Library
export PNPM_HOME="$TOOL/pnpm-home"
export NODE_USE_ENV_PROXY=1                  # Node 24 ignores HTTP(S)_PROXY otherwise (corporate/CI proxies)
export CI=true                               # keeps pnpm and tauri non-interactive
NODE_DIR="$TOOL/node-v$NODE_VERSION"
export PATH="$TOOL/bin:$TOOL/venv/bin:$NODE_DIR/bin:$CARGO_HOME/bin:$PATH"

# The repo's scripts start with `#!/usr/bin/env bash`, which resolves `bash` on
# PATH -- and /bin/bash (3.2) usually wins over Homebrew's. Re-exec'ing this
# script under bash 4+ does not help them, so put the bash that is running now
# first on PATH. A symlink, not /opt/homebrew/bin, so nothing else from Homebrew
# can shadow the pinned toolchain.
mkdir -p "$TOOL/bin"
ln -sf "$BASH" "$TOOL/bin/bash"

# ---- Prerequisites ------------------------------------------------------------
check_prerequisites() {
  step "Checking prerequisites"

  # The linker cannot replace a running executable, and the error it produces
  # does not name the app. Same reason build-windows-package.ps1 checks first.
  if pgrep -x uia >/dev/null 2>&1; then
    die "uia is running (PID $(pgrep -x uia | tr '\n' ' ')); close it and run this again"
  fi

  if ! xcode-select -p >/dev/null 2>&1 || ! xcrun --find clang >/dev/null 2>&1; then
    die "Xcode Command Line Tools are missing (linker and SDK). Run: xcode-select --install"
  fi

  command -v uv >/dev/null 2>&1 || die "uv not found; it provisions the Python toolchain (https://docs.astral.sh/uv/)"
  command -v curl >/dev/null 2>&1 || die "curl not found"
  note "Xcode CLT: $(xcode-select -p)"
  note "uv:        $(uv --version)"
}

# ---- Toolchain bootstrap ------------------------------------------------------
sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }

ensure_python() {
  if [ ! -x "$TOOL/venv/bin/python3" ]; then
    step "Provisioning Python $PYTHON_VERSION with uv"
    uv venv --python "$PYTHON_VERSION" "$TOOL/venv"
  fi
  python3 -c 'import tomllib' 2>/dev/null || die "Python in $TOOL/venv lacks tomllib; rm -rf $TOOL/venv and retry"
  note "python:    $(python3 --version) ($TOOL/venv)"
}

ensure_node() {
  if [ ! -x "$NODE_DIR/bin/node" ]; then
    step "Downloading Node $NODE_VERSION"
    local arch want
    case "$(uname -m)" in
      arm64)  arch="arm64"; want="$NODE_SHA256_ARM64" ;;
      x86_64) arch="x64";   want="$NODE_SHA256_X64" ;;
      *) die "unsupported architecture $(uname -m)" ;;
    esac
    local tarball="node-v$NODE_VERSION-darwin-$arch.tar.gz"
    mkdir -p "$TOOL"
    local tmp; tmp="$(mktemp -d "$TOOL/node-dl.XXXXXX")"
    curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/$tarball" -o "$tmp/$tarball"
    [ "$(sha256_of "$tmp/$tarball")" = "$want" ] || { rm -rf "$tmp"; die "checksum mismatch for $tarball; refusing to use it"; }
    tar -xzf "$tmp/$tarball" -C "$tmp"
    mv "$tmp/node-v$NODE_VERSION-darwin-$arch" "$NODE_DIR"
    rm -rf "$tmp"
  fi
  # Corepack ships inside Node and resolves the pnpm version from package.json.
  mkdir -p "$TOOL/bin"
  corepack enable --install-directory "$TOOL/bin" pnpm
  # Resolved into variables first: a failure inside $(...) in an argument list
  # does not trip `set -e`, and pnpm failing to download must stop the run.
  local node_v pnpm_v
  node_v="$(node --version)"
  pnpm_v="$(pnpm --version)" || die "pnpm could not be provisioned via Corepack (check network access to registry.npmjs.org)"
  note "node:      $node_v"
  note "pnpm:      $pnpm_v"
}

ensure_rust() {
  # Asks rustup whether a toolchain resolves, not whether a stub exists: an
  # interrupted install leaves rustc/cargo proxies behind with no toolchain.
  if ! rustc --version >/dev/null 2>&1; then
    step "Installing Rust ($RUST_CHANNEL) into $TOOL"
    local triple
    case "$(uname -m)" in
      arm64)  triple="aarch64-apple-darwin" ;;
      x86_64) triple="x86_64-apple-darwin" ;;
      *) die "unsupported architecture $(uname -m)" ;;
    esac
    mkdir -p "$TOOL"
    local tmp; tmp="$(mktemp -d "$TOOL/rustup-dl.XXXXXX")"
    local base="https://static.rust-lang.org/rustup/dist/$triple"
    curl -fsSL "$base/rustup-init" -o "$tmp/rustup-init"
    local want; want="$(curl -fsSL "$base/rustup-init.sha256" | awk '{print $1}')"
    [ "$(sha256_of "$tmp/rustup-init")" = "$want" ] || { rm -rf "$tmp"; die "checksum mismatch for rustup-init; refusing to run it"; }
    chmod +x "$tmp/rustup-init"
    # --no-modify-path: leave ~/.zshrc and ~/.profile alone.
    "$tmp/rustup-init" -y --no-modify-path --profile minimal \
      --default-toolchain "$RUST_CHANNEL" -c rustfmt -c clippy
    rm -rf "$tmp"
  fi
  if [ "$TARGET" = "universal-apple-darwin" ]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
  elif [ -n "$TARGET" ]; then
    rustup target add "$TARGET"
  fi
  local rust_v; rust_v="$(rustc --version)"
  note "rust:      $rust_v"
}

# ---- CI-equivalent checks (.github/workflows/ci.yaml) -------------------------
run_tests() {
  step "Repository checks"
  ./scripts/check-core-deps.sh
  ./scripts/check-webrtc-lockfile-pins.sh
  ./scripts/check-ipc-event-names.sh
  ./scripts/check-copyright-headers.sh
  ./scripts/tests/release/run-release-tests.sh
  ./scripts/tests/gen-notice/run-gen-notice-tests.sh

  # version.json, tauri.conf.json, Cargo.toml and Cargo.lock must agree.
  python3 scripts/release sync --check

  step "Rust: fmt, clippy, tests"
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test --workspace

  step "Frontend dependencies (frozen lockfile)"
  pnpm install --frozen-lockfile

  step "Desktop build check (the code the bundle ships)"
  # tauri::generate_context!() embeds dist/ at compile time, so build it first.
  pnpm build
  cargo check -p uia-app --features desktop --all-targets
  cargo clippy -p uia-app --features desktop --all-targets -- -D warnings

  # Opt-in. `cargo metadata` has no platform filter here (NOTICE.txt covers every
  # platform), so it downloads the Windows and Linux crates a macOS build never
  # compiles. If it fails with darwin entries unattributed, run
  # ./scripts/gen-notice.sh (a merge: it adds this platform and drops nothing)
  # and commit NOTICE.txt.
  if $WITH_NOTICE; then
    step "NOTICE.txt attribution"
    ./scripts/gen-notice.sh --check
  else
    step "Skipping NOTICE.txt attribution check (pass --with-notice to run it)"
  fi
}

# ---- Main ---------------------------------------------------------------------
check_prerequisites
step "Preparing toolchain in $TOOL"
ensure_python
ensure_node
ensure_rust

if $BOOTSTRAP_ONLY; then
  step "Toolchain ready"
  exit 0
fi

if $SKIP_TESTS; then
  step "Skipping tests (--skip-tests)"
  pnpm install --frozen-lockfile
else
  run_tests
fi

step "Building Tauri bundle (app, dmg)"
# tauri.conf.json declares only `msi`, so the bundles are named explicitly; the
# release workflow overrides it the same way for `deb`.
#
# Marker file instead of "a bundle exists": anything already on disk could be a
# leftover from an earlier run, and a failed build must not list a stale .dmg
# (the same trap build-windows-package.ps1 guards against).
mkdir -p "$TOOL"
MARKER="$TOOL/.build-started"
touch "$MARKER"
sleep 1   # mtimes can land in the same tick as the marker

tauri_args=(build --bundles app,dmg)
[ -n "$TARGET" ] && tauri_args+=(--target "$TARGET")
(cd crates/uia-app && pnpm exec tauri "${tauri_args[@]}")

if [ -n "$TARGET" ]; then
  BUNDLE_DIR="$REPO_ROOT/target/$TARGET/release/bundle"
else
  BUNDLE_DIR="$REPO_ROOT/target/release/bundle"
fi
[ -d "$BUNDLE_DIR" ] || BUNDLE_DIR="$REPO_ROOT/crates/uia-app/target/release/bundle"

artifacts="$(find "$BUNDLE_DIR" -maxdepth 2 \( -name '*.dmg' -o -name '*.app' \) -newer "$MARKER" 2>/dev/null || true)"
[ -n "$artifacts" ] || die "the build reported success but produced no .app or .dmg newer than this run in $BUNDLE_DIR"

printf '\n\033[32m==> Done. Bundle(s):\033[0m\n'
printf '  %s\n' $artifacts   # paths hold no spaces; the bundle dir is under the repo
if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  note "Unsigned build (APPLE_SIGNING_IDENTITY not set): fine on this Mac; Gatekeeper blocks it elsewhere."
fi
