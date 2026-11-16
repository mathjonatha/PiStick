#!/bin/sh
# Roda NO PI (Raspberry Pi OS): encerra o psd e os apps de teste anteriores e inicia o psd em modo
# Wayland com os apps pedidos, cada um com perfil limpo (pasta de dados própria dentro de ~/pistick/run).
# O mousepad pergunta se quer restaurar a sessão anterior quando a última foi encerrada à força; o
# perfil limpo evita essa janela extra e deixa os roteiros de tools/roteiros repetíveis.
#
# PSD_ARGS acrescenta argumentos ao psd (por exemplo PSD_ARGS='--video off'); PSD_VIDEO_OFF=1 é o mesmo
# para `--video off`, sem aspas (para chamar por ssh de um script do Windows); PSD_MAX_QP=N é `--video-max-qp N`.
# Uso:  ~/pistick/tools/pi/iniciar-psd.sh [foot] [mousepad] [mpv] [mpvgpu] [egl] [android]     (sem argumentos: foot e mousepad)
#
# Android (marco 5): `android` abre a tela inicial do Android do Waydroid numa janela (tools/pi/android/abrir-android.sh);
# ANDROID_APP=com.android.settings abre só esse app. Precisa de `sh tools/pi/instalar-android.sh` antes. O contêiner sobe
# quando o psd sobe o app e para quando o psd encerra. Som do Android: PSD_AUDIO=1. Veja docs/23.
#
# Áudio (marco 7): por padrão o psd roda com `--audio off` e o mpv com `--no-audio` (os roteiros de vídeo não
# mudam). PSD_AUDIO=1 (ou MPV_AUDIO=1) liga o áudio: o psd sobe o PipeWire do projeto (configuração em
# tools/pi/pipewire) num XDG_RUNTIME_DIR próprio (/run/user/UID/pistick-dev, para não disputar com o PipeWire
# da sessão do usuário), e o mpv toca com `--ao=pulse`. PSD_AUDIO_CODEC=pcm troca o codec do fio; PSD_AUDIO_RT=1 põe o PipeWire do projeto em tempo real (veja abaixo).
#
# Modo sintético (marco 4): se o primeiro argumento começa com `--`, todos os argumentos vão direto
# para o psd (sem --wayland e sem apps), por exemplo, o vídeo sintético que pausa depois de 12 s:
#       ~/pistick/tools/pi/iniciar-psd.sh --synthetic-video --synthetic-video-pause-after 12
#       ~/pistick/tools/pi/iniciar-psd.sh --synthetic-video --synthetic-video-pause-after 12 --video off
set -eu
cd "$HOME/pistick"

# Com o serviço pistick ativo, o psd dele ocupa a porta 7700: para o serviço antes (ver docs/15).
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

# Marco 9A: o psd de desenvolvimento usa a mesma identidade do serviço (chave do aparelho e lista de pareados em
# /var/lib/pistick, se a pasta existe e é nossa), para o notebook não ver "chave diferente" ao alternar entre os
# dois; e um soquete de controle em /run/user/UID (o `psd pair` o acha por XDG_RUNTIME_DIR numa sessão de login).
seg_args="--ctl-socket /run/user/$(id -u)/pistick-ctl"
if [ -d /var/lib/pistick ] && [ -w /var/lib/pistick ]; then
    seg_args="$seg_args --state-dir /var/lib/pistick"
fi

pkill -x psd 2>/dev/null || true
pkill -x foot 2>/dev/null || true
pkill -x mousepad 2>/dev/null || true
pkill -x mpv 2>/dev/null || true
# Os processos do PipeWire que um psd anterior subiu (pelos argumentos, para não tocar nos da sessão do usuário).
pkill -f 'pipewire -c .*pipewire.conf' 2>/dev/null || true
pkill -f 'pipewire-pulse -c ' 2>/dev/null || true
pkill -f 'wireplumber -p policy' 2>/dev/null || true
sleep 0.5
# O Android de um psd anterior para junto com ele (abrir-android.sh trata o SIGTERM): espera o contêiner sair e a ponte
# waydroid0 sumir (o daemon desfaz a ponte DEPOIS de parar o contêiner; um Android novo não pode começar no meio disso).
n=0
while { pgrep -x lxc-start || ip link show waydroid0; } >/dev/null 2>&1 && [ "$n" -lt 40 ]; do
    sleep 0.5
    n=$((n + 1))
done

case "${1:-}" in
    --*)
        # setsid: o psd sobrevive ao fim da sessão SSH.
        # shellcheck disable=SC2086
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
        # Clipe: $MPV_CLIP (padrão ~/clips/mandelbrot-720p30.mp4, o único que entra em vídeo); veja docs/06, "mpv (marco 4, etapa 8)".
        mpv) spawns="$spawns --spawn 'mpv --no-config --vo=wlshm --loop $mpv_audio --no-osc --no-input-default-bindings --really-quiet ${MPV_CLIP:-$HOME/clips/mandelbrot-720p30.mp4}'" ;;
        # Marco 4.5 (DMABUF): app EGL simples e mpv com --vo=gpu (V3D, buffers DMABUF). Veja docs/19. O
        # weston-simple-egl roda a 60 quadros por segundo (as fatias de quadro inteiro saem atômicas, docs/20);
        # EGL_ARGS acrescenta opções (por exemplo EGL_ARGS='-d 200000' para ~5 quadros por segundo).
        egl) spawns="$spawns --spawn 'weston-simple-egl ${EGL_ARGS:-}'" ;;
        # exec: o psd inicia cada app com `sh -c` e manda o SIGTERM só a esse sh; com o exec o script é o próprio processo
        # e recebe o sinal (para a sessão do Android e o contêiner). No serviço o systemd manda o sinal a todos.
        android) spawns="$spawns --spawn 'exec sh $HOME/pistick/tools/pi/android/abrir-android.sh ${ANDROID_APP:-}'" ;;
        mpvgpu) spawns="$spawns --spawn 'mpv --no-config --vo=gpu --gpu-context=wayland --loop $mpv_audio --no-osc --no-input-default-bindings --really-quiet ${MPV_CLIP:-$HOME/clips/mandelbrot-720p30.mp4}'" ;;
        *) spawns="$spawns --spawn $a" ;;
    esac
done

# setsid: o psd sobrevive ao fim da sessão SSH. O eval é intencional: $spawns traz aspas embutidas.
# shellcheck disable=SC2086
eval setsid -f ./target/release/psd --wayland --listen 0.0.0.0:7700 $seg_args $audio_arg ${PSD_ARGS:-} ${PSD_VIDEO_OFF:+--video off} ${PSD_MAX_QP:+--video-max-qp $PSD_MAX_QP} $spawns > "$HOME/psd.log" 2>&1
sleep 2
# Android: espera ele ficar pronto (o `waydroid` escreve "Android with user 0 is ready" no log do psd; uns 30 s) e a
# janela do app aparecer. Um psc que conecta antes vê uma janela provisória "Waydroid" fechar e sai.
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
# PSD_AUDIO_RT=1: prioridade de tempo real (SCHED_FIFO 20) para o PipeWire do projeto. Sem D-Bus não há RTKit,
# e com o mpv carregando a CPU o PipeWire perde ciclos (lacunas de 256 quadros no som). Só para medir: o serviço
# usa LimitRTPRIO na unidade (docs/16).
if [ -n "${PSD_AUDIO_RT:-}" ]; then
    for p in $(pgrep -f 'pipewire -c .*pipewire.conf') $(pgrep -f 'pipewire-pulse -c ') $(pgrep -f 'wireplumber -p policy'); do
        sudo chrt -a -f -p 20 "$p"
    done
    echo "PipeWire do projeto em SCHED_FIFO 20"
fi
cat "$HOME/psd.log"
volta
