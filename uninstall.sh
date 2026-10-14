#!/usr/bin/env bash
# Removes Systemd Manager from this computer.
#
#   bash uninstall.sh          find every installed copy, show it, ask, then remove
#   bash uninstall.sh -y       remove without asking
#   bash uninstall.sh --purge  also delete this folder's build output (target/)
#
# Services you created or edited with the app (unit files in /etc/systemd/system
# or ~/.config/systemd/user) are left alone.
set -euo pipefail

APP_ID="io.github.systemd_manager.SystemdManager"
BIN_NAME="systemd-manager"
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ASSUME_YES=0
PURGE=0

for arg in "$@"; do
    case "$arg" in
        -y|--yes) ASSUME_YES=1 ;;
        --purge) PURGE=1 ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; exit 1 ;;
    esac
done

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

# Every prefix install.sh could have used.
PREFIXES=("$HOME/.local" "/usr/local" "/usr")

FILES=()
FOUND_PREFIXES=()
for prefix in "${PREFIXES[@]}"; do
    found=0
    for f in "$prefix/bin/$BIN_NAME" \
             "$prefix/share/applications/$APP_ID.desktop" \
             "$prefix/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
             "$prefix/share/icons/hicolor/48x48/apps/$APP_ID.png" \
             "$prefix/share/icons/hicolor/256x256/apps/$APP_ID.png"; do
        if [[ -e "$f" ]]; then FILES+=("$f"); found=1; fi
    done
    [[ $found -eq 1 ]] && FOUND_PREFIXES+=("$prefix")
done
TARGET_DIR=""
[[ $PURGE -eq 1 && -d "$SRC_DIR/target" ]] && TARGET_DIR="$SRC_DIR/target"

if [[ ${#FILES[@]} -eq 0 && -z "$TARGET_DIR" ]]; then
    say "Systemd Manager is not installed; nothing to remove."
    exit 0
fi

say "This will remove:"
for f in "${FILES[@]}"; do echo "    $f"; done
[[ -n "$TARGET_DIR" ]] && echo "    $TARGET_DIR (build output)"

if [[ $ASSUME_YES -eq 0 ]]; then
    read -r -p "Continue? [y/N] " answer
    [[ "$answer" =~ ^[Yy]$ ]] || { echo "Cancelled."; exit 1; }
fi

pkill -x "$BIN_NAME" 2>/dev/null || true

# Use sudo only for files we can't delete ourselves.
remove() {
    if [[ -w "$(dirname "$1")" ]]; then rm -rf -- "$1"; else sudo rm -rf -- "$1"; fi
}
for f in "${FILES[@]}"; do remove "$f"; done
[[ -n "$TARGET_DIR" ]] && rm -rf -- "$TARGET_DIR"

# Refresh the app menu and icon caches so the entry disappears right away.
for prefix in "${FOUND_PREFIXES[@]}"; do
    apps="$prefix/share/applications"
    icons="$prefix/share/icons/hicolor"
    run=""
    [[ -w "$prefix/share" ]] || run="sudo"
    if [[ -d "$apps" ]] && command -v update-desktop-database >/dev/null; then
        $run update-desktop-database -q "$apps" 2>/dev/null || true
    fi
    if [[ -f "$icons/index.theme" || -f "$icons/icon-theme.cache" ]]; then
        for tool in gtk4-update-icon-cache gtk-update-icon-cache; do
            if command -v "$tool" >/dev/null; then
                $run "$tool" -q -t -f "$icons" 2>/dev/null || true
                break
            fi
        done
    fi
done

say "Systemd Manager has been removed."
