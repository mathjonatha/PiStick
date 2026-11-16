#!/bin/sh
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH
cd / || exit 0
umask 077
espera() {
    prazo=$(($1 * 2))
    shift
    n=0
    while "$@" >/dev/null 2>&1 && [ "$n" -lt "$prazo" ]; do
        sleep 0.5
        n=$((n + 1))
    done
}

espera 20 pgrep -x lxc-start
if pgrep -x lxc-start >/dev/null 2>&1; then
    timeout 40 waydroid container stop >/dev/null 2>&1 || true
fi
espera 10 ip link show waydroid0
systemctl stop waydroid-container.service >/dev/null 2>&1 || true
exit 0
