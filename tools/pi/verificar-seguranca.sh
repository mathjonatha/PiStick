#!/bin/sh
# Roda NO PI: auditoria de segurança do aparelho (marco 9B, docs/25). Só LÊ e relata; não muda nada.
# Uma linha por item: "ok", "aviso" ou "ABERTO". Sai com código 1 se houver ABERTO.
# Nunca imprime segredo (senha de Wi-Fi, chave, impressão digital, código de pareamento): só a existência.
#
#   sh tools/pi/verificar-seguranca.sh          auditoria do produto
#   sh tools/pi/verificar-seguranca.sh --dev    rebaixa a "aviso" o que é esperado no Pi de desenvolvimento
#                                               (sudo sem senha, senha do Wi-Fi no cartão)
set -eu

dev=0
for arg in "$@"; do
    case "$arg" in
        --dev) dev=1 ;;
        *)
            echo "uso: verificar-seguranca.sh [--dev]" >&2
            exit 2
            ;;
    esac
done

abertos=0
avisos=0
estado_dir=/var/lib/pistick

# ok|aviso|ABERTO ITEM [DETALHE]
linha() {
    if [ -n "${3:-}" ]; then
        printf '%-7s %s  (%s)\n' "$1" "$2" "$3"
    else
        printf '%-7s %s\n' "$1" "$2"
    fi
    case "$1" in
        ABERTO) abertos=$((abertos + 1)) ;;
        aviso) avisos=$((avisos + 1)) ;;
        *) ;;
    esac
}

# esperado_dev ITEM DETALHE: ABERTO no produto, aviso com --dev.
esperado_dev() {
    if [ "$dev" -eq 1 ]; then
        linha aviso "$1" "$2; esperado no Pi de desenvolvimento"
    else
        linha ABERTO "$1" "$2"
    fi
}

# modo_dono CAMINHO: imprime "MODO DONO" (stat do GNU coreutils).
modo_dono() {
    stat -c '%a %U' "$1" 2>/dev/null || echo "? ?"
}

# Confere modo e dono exatos de um caminho. exato ITEM CAMINHO MODO DONO
exato() {
    if [ ! -e "$2" ]; then
        linha aviso "$1" "não existe: $2"
        return
    fi
    atual=$(modo_dono "$2")
    if [ "$atual" = "$3 $4" ]; then
        linha ok "$1" "modo e dono corretos"
    else
        linha ABERTO "$1" "esperado $3 $4, está $atual"
    fi
}

echo "== Serviço =="
if ! systemctl cat pistick >/dev/null 2>&1; then
    linha aviso "serviço pistick" "unidade não instalada"
    usuario_servico=""
    pid=0
else
    usuario_servico=$(systemctl show pistick -p User --value 2>/dev/null || true)
    pid=$(systemctl show pistick -p MainPID --value 2>/dev/null || echo 0)
    if [ -z "$usuario_servico" ] || [ "$usuario_servico" = "root" ]; then
        linha ABERTO "usuário do serviço" "roda como root"
    else
        linha ok "usuário do serviço" "$usuario_servico"
    fi
    if [ "$(systemctl show pistick -p NoNewPrivileges --value 2>/dev/null)" = "yes" ]; then
        linha ok "NoNewPrivileges" "yes"
    else
        linha ABERTO "NoNewPrivileges" "a unidade não o liga"
    fi
    # Nota do systemd-analyze security (quanto menor, melhor; o projeto exige menos de 5,0).
    nota=$(systemd-analyze security pistick --no-pager 2>/dev/null | sed -n 's/.*Overall exposure level for pistick.service: \([0-9][0-9]*\.[0-9]\).*/\1/p' | tail -n 1)
    if [ -z "$nota" ]; then
        linha aviso "nota do systemd-analyze security" "não foi possível obter"
    elif awk -v n="$nota" 'BEGIN { exit !(n < 5.0) }'; then
        linha ok "nota do systemd-analyze security" "$nota (limite: menos de 5,0)"
    elif awk -v n="$nota" 'BEGIN { exit !(n < 7.0) }'; then
        linha aviso "nota do systemd-analyze security" "$nota (limite: menos de 5,0)"
    else
        linha ABERTO "nota do systemd-analyze security" "$nota (limite: menos de 5,0)"
    fi
    # sudo de dentro do serviço: com NoNewPrivs ligado no processo, o bit setuid do sudo não eleva nada.
    if [ "$pid" -gt 1 ] 2>/dev/null && [ -r "/proc/$pid/status" ]; then
        if [ "$(sed -n 's/^NoNewPrivs:[[:space:]]*//p' "/proc/$pid/status")" = "1" ]; then
            linha ok "sudo dentro do serviço" "bloqueado (NoNewPrivs=1 no processo $pid)"
        else
            linha ABERTO "sudo dentro do serviço" "o processo $pid pode ganhar privilégios"
        fi
    else
        linha aviso "sudo dentro do serviço" "serviço parado ou processo ilegível"
    fi
fi

echo "== Arquivos =="
if [ -d "$estado_dir" ]; then
    dono_estado=${usuario_servico:-$(id -un)}
    exato "pasta de estado $estado_dir" "$estado_dir" 700 "$dono_estado"
    for f in chave-do-aparelho pareados; do
        if [ -e "$estado_dir/$f" ]; then
            exato "$estado_dir/$f" "$estado_dir/$f" 600 "$dono_estado"
        else
            linha ok "$estado_dir/$f" "ainda não existe"
        fi
    done
else
    linha aviso "pasta de estado $estado_dir" "não existe (o serviço a cria ao subir)"
fi
# somente_root CAMINHO: dono root e sem escrita para grupo e outros.
somente_root() {
    m=$(stat -c '%a' "$1")
    d=$(stat -c '%U' "$1")
    if [ "$d" = "root" ] && [ "$(( 0$m & 022 ))" -eq 0 ]; then
        linha ok "$1" "root, modo $m"
    else
        linha ABERTO "$1" "dono $d, modo $m (esperado root, sem escrita para outros)"
    fi
}
if [ -d /etc/pistick ]; then
    somente_root /etc/pistick
    if [ -e /etc/pistick/psd.conf ]; then somente_root /etc/pistick/psd.conf; fi
else
    linha aviso "/etc/pistick" "não existe"
fi
exato "/usr/local/bin/psd" /usr/local/bin/psd 755 root root
if [ -d /usr/local/libexec/pistick ]; then
    exato "/usr/local/libexec/pistick (pasta)" /usr/local/libexec/pistick 755 root root
    for f in /usr/local/libexec/pistick/*; do
        [ -e "$f" ] || continue
        exato "$f" "$f" 755 root root
    done
else
    linha aviso "/usr/local/libexec/pistick" "não existe"
fi

echo "== Rede =="
# Escuta TCP: 7700 (qualquer interface), 22, e 4711/4712 só em laço local.
esperado_tcp=$(ss -tlnH 2>/dev/null | awk '{ print $4 }')
tcp_ok=1
for end in $esperado_tcp; do
    porta=${end##*:}
    host=${end%:*}
    case "$porta" in
        7700 | 22) ;;
        4711 | 4712)
            case "$host" in
                127.0.0.1 | "[::1]") ;;
                *)
                    linha ABERTO "porta $porta/TCP" "deveria escutar só em laço local, escuta em $host"
                    tcp_ok=0
                    ;;
            esac
            ;;
        *)
            linha aviso "porta $porta/TCP em escuta" "não está na lista esperada (7700, 22, 4711 e 4712 em laço)"
            tcp_ok=0
            ;;
    esac
done
if [ "$tcp_ok" -eq 1 ]; then
    linha ok "portas TCP em escuta" "só as esperadas (7700, 22; 4711 e 4712 só em laço local)"
fi
# UDP: 5353 (mDNS); portas altas (efêmeras) do próprio mDNS não contam.
udp_ok=1
for end in $(ss -ulnH 2>/dev/null | awk '{ print $4 }'); do
    porta=${end##*:}
    if [ "$porta" = "5353" ] || [ "$porta" -ge 32768 ] 2>/dev/null; then
        continue
    fi
    linha aviso "porta $porta/UDP em escuta" "não está na lista esperada (5353)"
    udp_ok=0
done
if [ "$udp_ok" -eq 1 ]; then
    linha ok "portas UDP em escuta" "só a 5353 (mDNS) e portas efêmeras"
fi
# psd sem --insecure (processo em execução e configuração do serviço)
inseguro=0
for p in $(pgrep -x psd 2>/dev/null || true); do
    if [ -r "/proc/$p/cmdline" ] && tr '\0' ' ' < "/proc/$p/cmdline" | grep -q -- '--insecure'; then
        inseguro=1
    fi
done
if [ -r /etc/pistick/psd.conf ] && grep -q -- '--insecure' /etc/pistick/psd.conf; then
    inseguro=1
fi
if [ "$inseguro" -eq 1 ]; then
    linha ABERTO "psd sem --insecure" "o psd aceita texto claro de qualquer origem"
else
    linha ok "psd sem --insecure" "texto claro só em laço local"
fi

echo "== Pareamento =="
lista=""
if [ -r "$estado_dir/pareados" ]; then
    lista=$estado_dir/pareados
    n=$(grep -c '^[0-9a-fA-F]' "$lista" || true)
elif [ -e "$estado_dir/pareados" ] && sudo -n true 2>/dev/null; then
    n=$(sudo -n grep -c '^[0-9a-fA-F]' "$estado_dir/pareados" || true)
    lista=sudo
elif [ -d "$estado_dir" ]; then
    n=0
    lista=vazia
else
    n=""
fi
if [ -z "$n" ]; then
    linha aviso "clientes pareados" "estado do serviço ilegível"
else
    linha ok "clientes pareados" "$n"
    if [ "$n" -eq 0 ]; then
        idade=""
        if [ "$pid" -gt 1 ] 2>/dev/null; then
            idade=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ' || true)
        fi
        if [ -n "$idade" ] && [ "$idade" -lt 600 ]; then
            linha aviso "primeiro uso do pareamento" "ABERTO por mais $((600 - idade)) s: o primeiro cliente que conectar é pareado"
        else
            linha ok "primeiro uso do pareamento" "fechado (sem pareados, mas o psd subiu há mais de 10 minutos; pistick-pair --reset reabre)"
        fi
    else
        linha ok "primeiro uso do pareamento" "fechado (há clientes pareados)"
    fi
fi

echo "== Conta e acesso =="
if [ "$(id -u)" -eq 0 ]; then
    linha aviso "sudo sem senha" "rodando como root; não foi possível conferir"
elif sudo -n true >/dev/null 2>&1; then
    esperado_dev "sudo sem senha para $(id -un)" "qualquer processo desse usuário vira root"
else
    linha ok "sudo sem senha" "$(id -un) não tem"
fi
# SSH com senha: lê as configurações (sshd -T exige root; tenta só se o sudo for sem senha).
senha_ssh=""
if sudo -n true >/dev/null 2>&1; then
    senha_ssh=$(sudo -n sshd -T 2>/dev/null | awk '$1 == "passwordauthentication" { print $2 }' || true)
fi
if [ -z "$senha_ssh" ]; then
    senha_ssh=$(cat /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf 2>/dev/null | awk 'tolower($1) == "passwordauthentication" { v = tolower($2) } END { print v }')
    [ -n "$senha_ssh" ] || senha_ssh=yes
fi
if [ "$senha_ssh" = "no" ]; then
    linha ok "SSH com senha" "desabilitado (só chave)"
else
    linha ABERTO "SSH com senha" "habilitado"
fi
# Senha do Wi-Fi em texto no cartão: só a existência, nunca o valor.
rede=/boot/firmware/network-config
if [ ! -e "$rede" ]; then
    linha ok "senha de Wi-Fi em $rede" "o arquivo não existe"
elif [ ! -r "$rede" ] && ! sudo -n true >/dev/null 2>&1; then
    linha aviso "senha de Wi-Fi em $rede" "arquivo ilegível para este usuário"
else
    if [ -r "$rede" ]; then
        achou=$(grep -c -i -E '^[[:space:]]*(password|psk):' "$rede" || true)
    else
        achou=$(sudo -n grep -c -i -E '^[[:space:]]*(password|psk):' "$rede" || true)
    fi
    if [ "${achou:-0}" -gt 0 ]; then
        esperado_dev "senha de Wi-Fi em texto em $rede" "existe (valor não mostrado)"
    else
        linha ok "senha de Wi-Fi em $rede" "não há senha em texto"
    fi
fi

echo
if [ "$abertos" -gt 0 ]; then
    echo "Resultado: $abertos ABERTO, $avisos aviso."
    exit 1
fi
echo "Resultado: nenhum ABERTO, $avisos aviso."
