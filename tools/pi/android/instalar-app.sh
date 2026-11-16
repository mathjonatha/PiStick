#!/bin/sh
set -eu

if [ $# -ne 1 ] || [ ! -f "$1" ]; then
    echo "uso: instalar-app.sh ARQUIVO.apk" >&2
    exit 2
fi
apk=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")

achou=""
for d in /run/pistick "/run/user/$(id -u)/pistick-dev" "/run/user/$(id -u)"; do
    for b in "$d"/android-bus.* "$d/bus"; do
        if [ -S "$b" ] && busctl --address="unix:path=$b" --no-pager list 2>/dev/null | grep -q 'id\.waydro\.Session'; then
            achou=$d
            barramento=$b
            break 2
        fi
    done
done
if [ -z "$achou" ]; then
    echo "instalar-app: não achei uma sessão do Waydroid no ar (suba com: sh tools/pi/iniciar-psd.sh android)." >&2
    exit 1
fi
XDG_RUNTIME_DIR=$achou
DBUS_SESSION_BUS_ADDRESS="unix:path=$barramento"
XDG_DATA_HOME="$HOME/.local/share"
export XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS XDG_DATA_HOME
exec waydroid app install "$apk"
