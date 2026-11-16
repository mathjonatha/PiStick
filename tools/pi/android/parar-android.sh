#!/bin/sh
# Roda COMO ROOT, no ExecStopPost do pistick.service (veja tools/pi/pistick.service.in): garante que o Android
# (Waydroid) fique parado quando o psd para. Instalado por instalar-android.sh em
# /usr/local/libexec/pistick/parar-android.sh; sem o Waydroid instalado a unidade ignora a falta do arquivo.
#
# Ao parar o serviço, o systemd manda SIGTERM a tudo que o psd iniciou; o `waydroid` da sessão (abrir-android.sh)
# pede ao waydroid-container, pelo D-Bus do sistema, que pare o contêiner. Quem desfaz a ponte waydroid0 e as
# tabelas nft é esse mesmo daemon, DEPOIS de parar o contêiner. Parar o daemon no meio disso (systemctl stop)
# deixaria a ponte e as regras para trás. Por isso este script ESPERA o pedido terminar e só então para o daemon:
#   1. espera até 20 s o lxc-start sair; se não saiu (o psd morreu sem passar pela sessão), pede a parada pelo
#      caminho normal (`waydroid container stop`);
#   2. espera até 10 s a ponte waydroid0 sumir (o fim da limpeza do daemon);
#   3. para o waydroid-container.service (o daemon ocioso que o D-Bus ativou).
#
# Revisão do marco 9B (docs/25): este é o ÚNICO processo do serviço que roda como root (`+` na unidade). Não recebe
# argumentos nem lê nada que o usuário do serviço escreva: o ambiente vem da unidade e de /etc/pistick/psd.conf (do
# root), o próprio script e a pasta dele são do root (modo 0755) e os comandos chamados são pgrep, waydroid, ip, sleep,
# timeout e systemctl, procurados num PATH fixo (abaixo).
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH
cd / || exit 0
umask 077
espera() { # espera() SEGUNDOS COMANDO...: roda o teste a cada 0,5 s até ele falhar ou o prazo acabar
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
