#!/bin/sh
n="${1:-30}"
cg=/sys/fs/cgroup/lxc.payload.waydroid
psd=$(pgrep -x psd | head -n 1)

ticks() { if [ -n "$1" ] && [ -r "/proc/$1/stat" ]; then awk '{print $14 + $15}' "/proc/$1/stat"; else echo 0; fi; }
usec() { if [ -r "$cg/cpu.stat" ]; then awk '/^usage_usec/{print $2}' "$cg/cpu.stat"; else echo 0; fi; }
ocupado() { awk '/^cpu /{print $2+$3+$4+$7+$8+$9}' /proc/stat; }

echo "== memória"
free -m | awk 'NR==1 || NR==2'
if [ -r "$cg/memory.current" ]; then
    echo "contêiner (cgroup): $(( $(cat "$cg/memory.current") / 1048576 )) MiB"
else
    echo "contêiner (cgroup): sem memory.current (controlador de memória desligado no kernel)"
fi
for p in psd pipewire wireplumber pipewire-pulse foot; do
    for pid in $(pgrep -x "$p"); do
        echo "$p ($pid): $(awk '/^VmRSS/{print int($2/1024)}' "/proc/$pid/status") MiB RSS"
    done
done
cont=$(ps -eo pid=,args= | awk '/python3 .usr.bin.waydroid container start/ && !/awk/ {print $1; exit}')
[ -n "$cont" ] && echo "daemon waydroid-container ($cont): $(awk '/^VmRSS/{print int($2/1024)}' "/proc/$cont/status") MiB RSS"

t0=$(ticks "$psd"); u0=$(usec); c0=$(ticks "$cont"); a0=$(ocupado); s0=$(date +%s.%N)
sleep "$n"
t1=$(ticks "$psd"); u1=$(usec); c1=$(ticks "$cont"); a1=$(ocupado); s1=$(date +%s.%N)
echo "== CPU em ${n}s (% de um núcleo)"
awk -v t0="$t0" -v t1="$t1" -v u0="$u0" -v u1="$u1" -v c0="$c0" -v c1="$c1" -v a0="$a0" -v a1="$a1" -v s0="$s0" -v s1="$s1" 'BEGIN {
    d = s1 - s0
    printf "psd=%.1f%% contêiner=%.1f%% daemon_waydroid_container=%.1f%% máquina=%.1f%% (de 400%%)\n", (t1-t0)/d, (u1-u0)/1e4/d, (c1-c0)/d, (a1-a0)/d
}'
