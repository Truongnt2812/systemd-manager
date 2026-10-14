#!/usr/bin/env bash
# Builds Systemd Manager and installs it with its desktop entry and icon.
#
#   ./install.sh                 install for all users into /usr/local (asks for sudo)
#   ./install.sh --user          install only for you into ~/.local (no sudo)
#   ./install.sh --prefix DIR    install into DIR
#   ./install.sh --uninstall     remove an installation (combine with --user/--prefix)
#   ./install.sh --no-deps       don't try to install missing build dependencies
set -euo pipefail

APP_ID="io.github.systemd_manager.SystemdManager"
BIN_NAME="systemd-manager"
MIN_RUST="1.92"
MIN_GTK="4.14"
MIN_ADW="1.5"

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="/usr/local"
USER_INSTALL=0
UNINSTALL=0
INSTALL_DEPS=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --user) USER_INSTALL=1; PREFIX="$HOME/.local" ;;
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift ;;
        --uninstall) UNINSTALL=1 ;;
        --no-deps) INSTALL_DEPS=0 ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Run a command with root rights only when the target isn't writable by us.
SUDO=""
needs_root() {
    local dir="$1"
    while [[ ! -e "$dir" ]]; do dir="$(dirname "$dir")"; done
    [[ ! -w "$dir" ]]
}
if [[ $EUID -ne 0 ]] && needs_root "$PREFIX"; then
    command -v sudo >/dev/null || die "$PREFIX is not writable and sudo is not available. Try --user."
    SUDO="sudo"
fi

BIN_PATH="$PREFIX/bin/$BIN_NAME"
DESKTOP_PATH="$PREFIX/share/applications/$APP_ID.desktop"
ICON_DIR="$PREFIX/share/icons/hicolor"
ICON_PATH="$ICON_DIR/scalable/apps/$APP_ID.svg"

refresh_caches() {
    if [[ -d "$PREFIX/share/applications" ]] && command -v update-desktop-database >/dev/null; then
        $SUDO update-desktop-database -q "$PREFIX/share/applications" || true
    fi
    [[ -d "$ICON_DIR" ]] || return 0
    if command -v gtk4-update-icon-cache >/dev/null; then
        $SUDO gtk4-update-icon-cache -q -t -f "$ICON_DIR" || true
    elif command -v gtk-update-icon-cache >/dev/null; then
        $SUDO gtk-update-icon-cache -q -t -f "$ICON_DIR" || true
    fi
}

if [[ $UNINSTALL -eq 1 ]]; then
    say "Removing Systemd Manager from $PREFIX"
    $SUDO rm -f "$BIN_PATH" "$DESKTOP_PATH" "$ICON_PATH" \
        "$ICON_DIR/48x48/apps/$APP_ID.png" "$ICON_DIR/256x256/apps/$APP_ID.png"
    refresh_caches
    say "Uninstalled."
    exit 0
fi

# ---- Build dependencies -----------------------------------------------------

version_ge() { [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]; }

missing_libs() {
    command -v pkg-config >/dev/null || return 0
    pkg-config --atleast-version="$MIN_GTK" gtk4 || return 0
    pkg-config --atleast-version="$MIN_ADW" libadwaita-1 || return 0
    command -v cc >/dev/null || return 0
    return 1
}

install_libs() {
    local root=""
    [[ $EUID -ne 0 ]] && root="sudo"
    if command -v apt-get >/dev/null; then
        $root apt-get install -y build-essential pkg-config libgtk-4-dev libadwaita-1-dev
    elif command -v dnf >/dev/null; then
        $root dnf install -y gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
    elif command -v pacman >/dev/null; then
        $root pacman -S --needed --noconfirm base-devel pkgconf gtk4 libadwaita
    elif command -v zypper >/dev/null; then
        $root zypper install -y gcc pkg-config gtk4-devel libadwaita-devel
    else
        die "Install the GTK 4 and libadwaita development packages, a C compiler and pkg-config, then run this again."
    fi
}

if missing_libs; then
    [[ $INSTALL_DEPS -eq 1 ]] || die "GTK >= $MIN_GTK and libadwaita >= $MIN_ADW development files are needed."
    say "Installing build dependencies (GTK 4, libadwaita, compiler)"
    install_libs
    missing_libs && die "Your distribution provides GTK $(pkg-config --modversion gtk4 2>/dev/null || echo '?') / libadwaita $(pkg-config --modversion libadwaita-1 2>/dev/null || echo '?'), but GTK >= $MIN_GTK and libadwaita >= $MIN_ADW are required."
fi

# Prefer rustup's cargo if it is installed but not on PATH yet.
[[ -x "$HOME/.cargo/bin/cargo" ]] && export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || die "Rust is not installed. Install it from https://rustup.rs and run this again."
RUST_VERSION="$(rustc --version | awk '{print $2}')"
if ! version_ge "$RUST_VERSION" "$MIN_RUST"; then
    if command -v rustup >/dev/null; then
        say "Updating Rust ($RUST_VERSION is older than $MIN_RUST)"
        rustup update stable
    else
        die "Rust $RUST_VERSION is too old; $MIN_RUST or newer is needed. Install rustup from https://rustup.rs."
    fi
fi

# ---- Build and install --------------------------------------------------------

if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" ]]; then
    warn "Running under sudo; the build will run as root. Running ./install.sh without sudo is preferred."
fi

say "Building (release)"
(cd "$SRC_DIR" && cargo build --release --locked)

say "Installing into $PREFIX"
$SUDO install -Dm755 "$SRC_DIR/target/release/$BIN_NAME" "$BIN_PATH"
$SUDO install -Dm644 "$SRC_DIR/data/$APP_ID.svg" "$ICON_PATH"
for size in 48x48 256x256; do
    $SUDO install -Dm644 "$SRC_DIR/data/icons/$size.png" "$ICON_DIR/$size/apps/$APP_ID.png"
done
# Use the absolute binary path so the launcher works even if $PREFIX/bin isn't on PATH.
sed "s|^Exec=.*|Exec=$BIN_PATH|" "$SRC_DIR/data/$APP_ID.desktop" \
    | $SUDO install -Dm644 /dev/stdin "$DESKTOP_PATH"
refresh_caches

say "Installed Systemd Manager."
echo "    Launch it from your app menu, or run: $BIN_PATH"
if [[ ":$PATH:" != *":$PREFIX/bin:"* ]]; then
    echo "    ($PREFIX/bin is not on your PATH, so use the full path or the app menu.)"
fi
