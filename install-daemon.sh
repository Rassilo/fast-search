#!/bin/sh
# Installs (or removes) ffd, the Fast search index daemon, as a systemd
# service. Build first with: cargo build --release
set -eu

usage() {
    cat <<'EOF'
Usage: ./install-daemon.sh [-g GROUP]    install and start the ffd service
       ./install-daemon.sh --uninstall   stop and remove it

  -g, --group GROUP   group allowed to read the index
                      (default: the primary group of the user running this)

Everyone in GROUP can search the names of ALL files on the system,
including files in folders they cannot open.
EOF
}

action=install
group=
while [ $# -gt 0 ]; do
    case "$1" in
        -g|--group)
            [ $# -ge 2 ] || { echo "install-daemon: $1 needs a group name" >&2; exit 2; }
            group=$2
            shift 2
            ;;
        --uninstall) action=uninstall; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install-daemon: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
bin="$here/target/x86_64-unknown-linux-musl/release/ffd"
unit=/etc/systemd/system/ffd.service
installed=/usr/local/bin/ffd

command -v systemctl >/dev/null 2>&1 || { echo "install-daemon: systemd (systemctl) is required" >&2; exit 1; }

if [ "$action" = install ]; then
    # Check what we can before asking for root.
    if [ ! -x "$bin" ]; then
        echo "install-daemon: $bin not found; run 'cargo build --release' first" >&2
        exit 1
    fi
    if [ -z "$group" ]; then
        user=${SUDO_USER:-$(id -un)}
        if [ "$user" = root ]; then
            echo "install-daemon: running as root; name the reader group with -g GROUP" >&2
            exit 2
        fi
        group=$(id -gn "$user")
    fi
fi

# Everything below needs root: re-run this script through sudo.
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || { echo "install-daemon: run this as root" >&2; exit 1; }
    if [ "$action" = install ]; then
        exec sudo sh "$here/$(basename -- "$0")" --group "$group"
    fi
    exec sudo sh "$here/$(basename -- "$0")" --uninstall
fi

if [ "$action" = uninstall ]; then
    # Prefer the installed copy; fall back to cleaning up by hand.
    if [ -x "$installed" ]; then
        "$installed" --uninstall-service
    else
        systemctl disable --now ffd 2>/dev/null || true
        rm -f "$unit" "$installed"
        rm -rf /run/ff
        systemctl daemon-reload
    fi
    exit 0
fi

echo "Installing ffd; group '$group' will be able to read the index."
"$bin" --install-service "$group"   # installs, enables and starts the service

# Give it a moment to scan, then show what it logged.
sleep 2
systemctl --no-pager --lines=12 status ffd || true
if systemctl is-active --quiet ffd; then
    echo
    echo "ffd is running. Restart ff-gui so it switches to the live index."
else
    echo
    echo "ffd did not stay running; see: journalctl -u ffd" >&2
    exit 1
fi
