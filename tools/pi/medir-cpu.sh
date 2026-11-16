#!/bin/sh
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
