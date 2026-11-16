#!/bin/sh
set -eu
cd "$HOME/pistick"

servico_parado=0
if systemctl is-active --quiet pistick 2>/dev/null; then
    echo "Serviço pistick ativo: parando (sudo systemctl stop pistick) para usar o psd de desenvolvimento."
    sudo systemctl stop pistick
    servico_parado=1
fi
volta() {
    if [ "$servico_parado" -eq 1 ]; then
        echo "Para voltar ao serviço: pkill -x psd; sudo systemctl start pistick"
    fi
}

apps="${*:-foot mousepad}"

seg_args="--ctl-socket /run/user/$(id -u)/pistick-ctl"
if [ -d /var/lib/pistick ] && [ -w /var/lib/pistick ]; then
    seg_args="$seg_args --state-dir /var/lib/pistick"
fi

pkill -x psd 2>/dev/null || true
pkill -x foot 2>/dev/null || true
pkill -x mousepad 2>/dev/null || true
pkill -x mpv 2>/dev/null || true
pkill -f 'pipewire -c .*pipewire.conf' 2>/dev/null || true
pkill -f 'pipewire-pulse -c ' 2>/dev/null || true
pkill -f 'wireplumber -p policy' 2>/dev/null || true
sleep 0.5
n=0
while { pgrep -x lxc-start || ip link show waydroid0; } >/dev/null 2>&1 && [ "$n" -lt 40 ]; do
    sleep 0.5
    n=$((n + 1))
done

case "${1:-}" in
    --*)
        setsid -f ./target/release/psd --listen 0.0.0.0:7700 $seg_args "$@" > "$HOME/psd.log" 2>&1 < /dev/null
        sleep 1
        cat "$HOME/psd.log"
        volta
        exit 0
        ;;
esac

rm -rf run
mkdir -p run/cache run/config run/data

audio_arg="--audio off"
mpv_audio="--no-audio"
if [ -n "${PSD_AUDIO:-}${MPV_AUDIO:-}" ]; then
    XDG_RUNTIME_DIR="/run/user/$(id -u)/pistick-dev"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    export XDG_RUNTIME_DIR
    audio_arg="--audio auto --audio-codec ${PSD_AUDIO_CODEC:-psa1}"
    mpv_audio="--ao=pulse"
fi
export XDG_CACHE_HOME="$PWD/run/cache" XDG_CONFIG_HOME="$PWD/run/config" XDG_DATA_HOME="$PWD/run/data"

spawns=""
for a in $apps; do
    case "$a" in
        mousepad) spawns="$spawns --spawn 'mousepad --disable-server'" ;;
        mpv) spawns="$spawns --spawn 'mpv --no-config --vo=wlshm --loop $mpv_audio --no-osc --no-input-default-bindings --really-quiet ${MPV_CLIP:-$HOME/clips/mandelbrot-720p30.mp4}'" ;;
        egl) spawns="$spawns --spawn 'weston-simple-egl ${EGL_ARGS:-}'" ;;
        android) spawns="$spawns --spawn 'exec sh $HOME/pistick/tools/pi/android/abrir-android.sh ${ANDROID_APP:-}'" ;;
        mpvgpu) spawns="$spawns --spawn 'mpv --no-config --vo=gpu --gpu-context=wayland --loop $mpv_audio --no-osc --no-input-default-bindings --really-quiet ${MPV_CLIP:-$HOME/clips/mandelbrot-720p30.mp4}'" ;;
        *) spawns="$spawns --spawn $a" ;;
    esac
done

eval setsid -f ./target/release/psd --wayland --listen 0.0.0.0:7700 $seg_args $audio_arg ${PSD_ARGS:-} ${PSD_VIDEO_OFF:+--video off} ${PSD_MAX_QP:+--video-max-qp $PSD_MAX_QP} $spawns > "$HOME/psd.log" 2>&1
sleep 2
case " $apps " in
    *" android "*)
        echo "Esperando o Android ficar pronto (até 2 min)..."
        n=0
        while ! grep -q "is ready" "$HOME/psd.log" 2>/dev/null && [ "$n" -lt 240 ]; do
            sleep 0.5
            n=$((n + 1))
        done
        sleep 6
        ;;
esac
if [ -n "${PSD_AUDIO_RT:-}" ]; then
    for p in $(pgrep -f 'pipewire -c .*pipewire.conf') $(pgrep -f 'pipewire-pulse -c ') $(pgrep -f 'wireplumber -p policy'); do
        sudo chrt -a -f -p 20 "$p"
    done
    echo "PipeWire do projeto em SCHED_FIFO 20"
fi
cat "$HOME/psd.log"
volta
