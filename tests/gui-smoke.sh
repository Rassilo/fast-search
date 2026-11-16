#!/bin/sh
# Drawing test of the window, ff-gui, on a real display server.
#
# It opens the window once per view (results, menus, dialogs, folder
# sizes...). Each time the window must connect with the expected backend,
# draw itself, save that picture and exit cleanly; the picture must not
# be blank and must differ from the plain results view. No keys or mouse
# are involved: this catches a window that cannot start on a display
# server, crashes while drawing, or draws nothing.
#
#   tests/gui-smoke.sh x11        on the X server in $DISPLAY
#   tests/gui-smoke.sh wayland    on the compositor in $WAYLAND_DISPLAY
#
# Without a display of your own (as in CI), it can start one that needs
# no screen, keyboard or graphics card:
#   xvfb-run -a tests/gui-smoke.sh x11
#   tests/gui-smoke.sh wayland sway       also: labwc, weston, mutter
#
# KEEP=1 keeps the pictures (PPM files) and logs; their folder is printed.
set -u

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
bin=${FF_BIN:-"$here/../target/x86_64-unknown-linux-musl/release"}
backend=${1:-}
case $backend in
    x11) name=X11 ;;
    wayland) name=Wayland ;;
    *) echo "usage: $0 x11|wayland [sway|labwc|weston|mutter]" >&2; exit 2 ;;
esac
for program in ff ff-gui; do
    [ -x "$bin/$program" ] || { echo "$bin/$program is missing: run cargo build --release first" >&2; exit 2; }
done

work=$(mktemp -d)
compositor=${2:-}
compositor_pid=
cleanup() {
    [ -n "$compositor_pid" ] && kill "$compositor_pid" 2>/dev/null
    if [ -n "${KEEP:-}" ]; then echo "kept: $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

if [ -n "$compositor" ]; then
    export XDG_RUNTIME_DIR="$work/run"
    mkdir -m 700 "$XDG_RUNTIME_DIR"
    unset DISPLAY WAYLAND_DISPLAY
    # wlroots compositors: no screen, no input devices, drawn by the CPU.
    export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
    case $compositor in
        sway)
            printf 'xwayland disable\noutput HEADLESS-1 resolution 1280x1024\n' > "$work/sway.conf"
            # The flag only matters where the Nvidia driver is loaded: Sway
            # refuses to start there, though nothing here uses the card.
            sway --unsupported-gpu -c "$work/sway.conf" > "$work/compositor.log" 2>&1 &
            ;;
        labwc)
            mkdir "$work/labwc"
            labwc -C "$work/labwc" > "$work/compositor.log" 2>&1 &
            ;;
        weston)
            weston --backend=headless --width=1280 --height=1024 > "$work/compositor.log" 2>&1 &
            ;;
        mutter) # GNOME's compositor
            dbus-run-session -- mutter --headless --wayland --no-x11 --virtual-monitor 1280x1024 > "$work/compositor.log" 2>&1 &
            ;;
        *) echo "unknown compositor: $compositor" >&2; exit 2 ;;
    esac
    compositor_pid=$!
    tries=0
    until socket=$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | grep -m 1 '^wayland-[0-9]*$'); do
        tries=$((tries + 1))
        if [ $tries -gt 150 ] || ! kill -0 "$compositor_pid" 2>/dev/null; then
            echo "$compositor did not start:" >&2
            cat "$work/compositor.log" >&2
            exit 1
        fi
        sleep 0.1
    done
    export WAYLAND_DISPLAY="$socket"
    echo "$compositor is up on $socket"
fi

# The window needs an index to show: a small one of this repository's
# sources, kept in the temporary folder.
mkdir -p "$work/config"
export FF_DB="$work/index.db"
"$bin/ff" index "$here/../crates" > /dev/null 2>&1 || { echo "FAIL: could not prepare an index"; exit 1; }

# Settings and scale must not depend on who runs this.
export XDG_CONFIG_HOME="$work/config"
export FF_GUI_SCALE=1
export FF_GUI_BACKEND=$backend

failed=0
plain=
# The first is the plain results view; the others must look different.
for view in results menu bar about props prefs options1 options4 drives help0 regex tree complete; do
    picture="$work/$view.ppm"
    log="$work/$view.log"
    hook=$view
    [ "$view" = results ] && hook=
    FF_GUI_TEST=$hook FF_GUI_SNAPSHOT="$picture" FF_GUI_DEBUG="$log" \
        timeout 30 "$bin/ff-gui" main > "$work/$view.out" 2>&1
    status=$?
    problem=
    if [ $status -ne 0 ]; then
        problem="exit status $status: $(tail -n 3 "$work/$view.out" | tr '\n' ' ')"
    elif ! grep -q "^display: $name " "$log" 2>/dev/null; then
        problem="did not use the $name backend: $(grep '^display' "$log" 2>/dev/null)"
    elif [ "$(head -c 2 "$picture" 2>/dev/null)" != P6 ]; then
        problem="saved no picture"
    elif [ "$(tr -d '\377' < "$picture" | wc -c)" -lt 5000 ]; then
        problem="the picture is blank"
    else
        sum=$(cksum < "$picture")
        case $view in
            results) plain=$sum ;;
            # A regex search for the same word finds the same results. An
            # open menu is closed again when the compositor gives the new
            # window its size, so menus are only checked for drawing.
            regex | menu | bar) ;;
            *) [ "$sum" = "$plain" ] && problem="looks the same as the plain results view" ;;
        esac
    fi
    if [ -n "$problem" ]; then
        echo "FAIL $backend $view: $problem"
        failed=1
    else
        echo "ok   $backend $view"
    fi
done

if [ $failed -ne 0 ] && [ -n "$compositor_pid" ]; then
    echo "--- $compositor log ---"
    tail -n 30 "$work/compositor.log"
fi
exit $failed
