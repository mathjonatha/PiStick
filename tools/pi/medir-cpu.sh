#!/bin/sh
# Roda NO PI: amostra a CPU acumulada (utime + stime, em ticks de 1/100 s) do psd e de um segundo
# processo (padrão mousepad; o marco 4 usa mpv) a cada segundo, por N segundos.
# Saída: "segundo ticks_psd ticks_outro".
# Um valor de 100 ticks por segundo é um núcleo inteiro (o Pi 4 tem 4).
# Uso: sh ~/pistick/tools/pi/medir-cpu.sh 30 [processo]
n="${1:-30}"
other="${2:-mousepad}"
ticks() {
    pid=$(pgrep -x "$1" | head -n 1)
    if [ -n "$pid" ]; then awk '{print $14 + $15}' "/proc/$pid/stat"; else echo 0; fi
}
i=0
while [ "$i" -le "$n" ]; do
    echo "$i $(ticks psd) $(ticks "$other")"
    i=$((i + 1))
    sleep 1
done
