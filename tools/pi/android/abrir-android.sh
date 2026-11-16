#!/bin/sh
# Roda NO PI, como um app do psd (APP_n no /etc/pistick/psd.conf, ou `iniciar-psd.sh android`): sobe o
# Android do Waydroid contra o compositor do psd e mostra uma janela.
#
#   abrir-android.sh              a tela inicial do Android inteira, numa janela (waydroid show-full-ui)
#   abrir-android.sh PACOTE       só o app (waydroid app launch PACOTE), por exemplo com.android.settings
#
# O que faz, na ordem:
#   1. confere o ambiente que o psd dá aos apps (XDG_RUNTIME_DIR e WAYLAND_DISPLAY);
#   2. garante um barramento D-Bus de sessão (o `waydroid` fala por ele com a própria sessão); no serviço
#      do psd não há sessão de login, então sobe um dbus-daemon privado em $XDG_RUNTIME_DIR/android-bus.PID;
#   3. roda `waydroid app launch` ou `waydroid show-full-ui`. Se não há sessão, esse comando a inicia e
#      fica em primeiro plano enquanto ela existir; o contêiner (waydroid-container.service, root) é
#      ativado pelo D-Bus do sistema, sem sudo, e só existe enquanto a sessão existir;
#   4. ao receber SIGTERM/SIGINT/SIGHUP (o psd manda SIGTERM aos apps quando encerra), pede à sessão que
#      pare, o que para o contêiner, e só então sai.
#
# O contêiner só roda quando o Android é pedido. Um SIGKILL no psd não passa por aqui; nesse caso o
# systemd, ao parar o serviço, manda SIGTERM a todos os processos dele, e o resultado é o mesmo.
# Veja docs/23-android-instalacao-e-uso.md.
set -eu

if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "abrir-android: faltam XDG_RUNTIME_DIR e WAYLAND_DISPLAY (este script é um app do psd)." >&2
    exit 1
fi
if ! command -v waydroid >/dev/null 2>&1; then
    echo "abrir-android: waydroid não está instalado (sh tools/pi/instalar-android.sh)." >&2
    exit 1
fi
# O psd de desenvolvimento (iniciar-psd.sh) aponta XDG_DATA_HOME para uma pasta que é apagada a cada
# partida; os dados do Android (apps instalados, contas) ficam em ~/.local/share/waydroid.
XDG_DATA_HOME="$HOME/.local/share"
export XDG_DATA_HOME

# Barramento de sessão: se XDG_RUNTIME_DIR é o de um login (o systemd cria `bus` nele), usa esse. Senão (serviço, ou o
# psd de desenvolvimento com PSD_AUDIO=1) sobe um dbus-daemon PRÓPRIO, com o PID no nome do soquete: um psd novo que
# começa enquanto o anterior ainda está parando não pode reaproveitar o barramento que o outro vai derrubar.
# (instalar-app.sh acha a sessão em `bus` ou em `android-bus.*`.)
bus="$XDG_RUNTIME_DIR/bus"
bus_pid=""
if [ -S "$bus" ] && busctl --address="unix:path=$bus" --no-pager status >/dev/null 2>&1; then
    : # barramento de sessão de um login (SSH): usa
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

# Para a sessão (o que para o contêiner) e o barramento que subimos. Espera até 20 s pelo `waydroid`;
# se ele não sair, pede a parada pelo caminho normal e, por fim, o mata.
# shellcheck disable=SC2317 # chamada pelo trap
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
        kill "$bus_pid" 2>/dev/null || true # o dbus-daemon apaga o próprio soquete ao sair
    fi
    exit 0
}
trap parar TERM INT HUP

# `wait` é interrompido pelo sinal; se o `waydroid` sair sozinho (sessão parada por outro meio), limpa e sai.
status=0
wait "$pid" || status=$?
if [ -n "$bus_pid" ]; then
    kill "$bus_pid" 2>/dev/null || true
fi
exit "$status"
