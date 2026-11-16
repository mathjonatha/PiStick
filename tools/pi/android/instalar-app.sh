#!/bin/sh
# Roda NO PI: instala um APK no Android do Waydroid que o psd está rodando.
#
#   sh tools/pi/android/instalar-app.sh ARQUIVO.apk
#   sh tools/pi/android/instalar-app.sh tools/pi/android/tom.apk     # o app de teste de som (roteiro 102)
#
# O Android tem de estar no ar (psd com abrir-android.sh: `sh tools/pi/iniciar-psd.sh android`, ou o serviço com
# um APP_n do Android). O script acha o barramento D-Bus de sessão onde a sessão do Waydroid está registrada.
# O app fica nos dados do Android (~/.local/share/waydroid) e sobrevive aos reinícios; `instalar-android.sh
# --remover` o apaga junto com o resto.
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
