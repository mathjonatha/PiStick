#!/bin/sh
set -eu

if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "abrir-android: faltam XDG_RUNTIME_DIR e WAYLAND_DISPLAY (este script é um app do psd)." >&2
    exit 1
fi
if ! command -v waydroid >/dev/null 2>&1; then
    echo "abrir-android: waydroid não está instalado (sh tools/pi/instalar-android.sh)." >&2
    exit 1
fi
XDG_DATA_HOME="$HOME/.local/share"
export XDG_DATA_HOME

bus="$XDG_RUNTIME_DIR/bus"
bus_pid=""
if [ -S "$bus" ] && busctl --address="unix:path=$bus" --no-pager status >/dev/null 2>&1; then
    :
else
    bus="$XDG_RUNTIME_DIR/android-bus.$$"
    rm -f "$bus"
    dbus-daemon --session --address="unix:path=$bus" --nofork --nopidfile &
    bus_pid=$!
    n=0
    while [ ! -S "$bus" ] && [ "$n" -lt 50 ]; do
        sleep 0.1
        n=$((n + 1))
    done
    if [ ! -S "$bus" ]; then
        echo "abrir-android: o dbus-daemon não criou $bus" >&2
        kill "$bus_pid" 2>/dev/null || true
        exit 1
    fi
fi
DBUS_SESSION_BUS_ADDRESS="unix:path=$bus"
export DBUS_SESSION_BUS_ADDRESS

if [ -n "${1:-}" ]; then
    waydroid app launch "$1" &
else
    waydroid show-full-ui &
fi
pid=$!

parar() {
    trap '' TERM INT HUP
    kill -TERM "$pid" 2>/dev/null || true
    n=0
    while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 200 ]; do
        sleep 0.1
        n=$((n + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        timeout 15 waydroid session stop >/dev/null 2>&1 || true
        kill -KILL "$pid" 2>/dev/null || true
    fi
    if [ -n "$bus_pid" ]; then
        kill "$bus_pid" 2>/dev/null || true
    fi
    exit 0
}
trap parar TERM INT HUP

status=0
wait "$pid" || status=$?
if [ -n "$bus_pid" ]; then
    kill "$bus_pid" 2>/dev/null || true
fi
exit "$status"
