#!/bin/sh

PISTICK_BASE_URL="${PISTICK_BASE_URL:-}"

set -eu

modo=instalar
pacote=""
usuario=""
iniciar=1

uso() {
    cat >&2 <<'USO'
uso: instalar.sh [--pacote ARQUIVO.tar.xz] [--usuario NOME] [--sem-iniciar]
     instalar.sh --verificar [--pacote ARQUIVO.tar.xz] [--usuario NOME]
     instalar.sh --desinstalar
USO
}

while [ $# -gt 0 ]; do
    case "$1" in
        --pacote)
            [ $# -ge 2 ] || { uso; exit 2; }
            pacote=$2
            shift 2
            ;;
        --usuario)
            [ $# -ge 2 ] || { uso; exit 2; }
            usuario=$2
            shift 2
            ;;
        --sem-iniciar)
            iniciar=0
            shift
            ;;
        --verificar)
            modo=verificar
            shift
            ;;
        --desinstalar)
            modo=desinstalar
            shift
            ;;
        -h | --ajuda | --help)
            uso
            exit 0
            ;;
        *)
            uso
            exit 2
            ;;
    esac
done

destino_psd=/usr/local/bin/psd
destino_libexec=/usr/local/libexec/pistick
destino_share=/usr/local/share/pistick
dir_conf=/etc/pistick
conf=$dir_conf/psd.conf
unidade=/etc/systemd/system/pistick.service
unidade_pb=/etc/systemd/system/pistick-primeiro-boot.service

falhas=0
tmp=""
limpar() {
    if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}
trap limpar EXIT

estado() {
    printf '%-7s %s' "$2" "$1"
    if [ -n "${3:-}" ]; then printf '  (%s)' "$3"; fi
    printf '\n'
}

falta() {
    estado "$1" FALTA "${2:-}"
    falhas=1
}

falhou() {
    estado "$1" FALHOU "${2:-}" >&2
    exit 1
}

sd=0
if [ -d /run/systemd/system ]; then sd=1; fi

exigir_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "FALHOU: rode como root (sudo sh instalar.sh ...)." >&2
        exit 1
    fi
}

checar_sistema() {
    if [ "$(uname -m)" != "aarch64" ]; then
        echo "FALHOU: o PiStick só roda em sistemas de 64 bits ARM (aarch64); este é $(uname -m)." >&2
        exit 1
    fi
    if [ ! -r /etc/os-release ]; then
        echo "FALHOU: não achei /etc/os-release; só Debian 12/13 e Raspberry Pi OS são suportados." >&2
        exit 1
    fi
    so_id=$(. /etc/os-release && printf '%s' "${ID:-}")
    so_like=$(. /etc/os-release && printf '%s' "${ID_LIKE:-}")
    so_versao=$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")
    case " $so_id $so_like " in
        *" debian "* | *" raspbian "*) ;;
        *)
            echo "FALHOU: sistema '$so_id' não suportado; só Debian 12/13 e Raspberry Pi OS." >&2
            exit 1
            ;;
    esac
    case "$so_versao" in
        12 | 13) ;;
        *)
            echo "FALHOU: versão '$so_versao' não suportada; só Debian 12 ou 13 (Raspberry Pi OS equivalente)." >&2
            exit 1
            ;;
    esac
}

conferir_sha() {
    esperado=$(awk '{ print tolower($1); exit }' "$2")
    case "$esperado" in
        '' | *[!0-9a-f]*) return 1 ;;
    esac
    [ "${#esperado}" -eq 64 ] || return 1
    real=$(sha256sum "$1" | awk '{ print $1 }')
    [ "$esperado" = "$real" ]
}

obter_pacote() {
    tmp=$(mktemp -d)
    arq=$pacote
    if [ -n "$pacote" ]; then
        [ -f "$pacote" ] || { echo "FALHOU: o pacote '$pacote' não existe." >&2; exit 1; }
        if [ -f "$pacote.sha256" ]; then
            if conferir_sha "$pacote" "$pacote.sha256"; then
                estado "pacote: SHA-256" ok "$pacote.sha256"
            else
                echo "FALHOU: o SHA-256 de '$pacote' não confere com '$pacote.sha256'; pacote recusado." >&2
                exit 1
            fi
        else
            estado "pacote: SHA-256" ok "sem .sha256 ao lado; não conferido"
        fi
    elif [ -n "$PISTICK_BASE_URL" ]; then
        if ! command -v curl >/dev/null 2>&1; then
            echo "FALHOU: o curl não está instalado (sudo apt-get install curl)." >&2
            exit 1
        fi
        arq="$tmp/pistick-latest-aarch64.tar.xz"
        curl --proto '=https' --tlsv1.2 -fsSL -o "$arq" "$PISTICK_BASE_URL/pistick-latest-aarch64.tar.xz" ||
            { echo "FALHOU: não consegui baixar o pacote de $PISTICK_BASE_URL." >&2; exit 1; }
        curl --proto '=https' --tlsv1.2 -fsSL -o "$arq.sha256" "$PISTICK_BASE_URL/pistick-latest-aarch64.tar.xz.sha256" ||
            { echo "FALHOU: não consegui baixar o .sha256 de $PISTICK_BASE_URL." >&2; exit 1; }
        if conferir_sha "$arq" "$arq.sha256"; then
            estado "pacote: SHA-256" ok "baixado e conferido"
        else
            echo "FALHOU: o SHA-256 do pacote baixado não confere; pacote recusado." >&2
            exit 1
        fi
    else
        echo "FALHOU: sem --pacote e sem endereço de download (PISTICK_BASE_URL vazio)." >&2
        echo "        Use: sudo sh instalar.sh --pacote pistick-<versão>-aarch64.tar.xz" >&2
        exit 1
    fi

    tar -tf "$arq" > "$tmp/lista" 2> "$tmp/erro" ||
        { echo "FALHOU: '$arq' não é um pacote .tar.xz legível." >&2; exit 1; }
    if [ ! -s "$tmp/lista" ]; then
        echo "FALHOU: pacote vazio." >&2
        exit 1
    fi
    while IFS= read -r entrada; do
        case "$entrada" in
            /* | .. | ../* | */.. | */../*)
                echo "FALHOU: o pacote tem uma entrada com caminho perigoso ('$entrada'); recusado." >&2
                exit 1
                ;;
        esac
    done < "$tmp/lista"
    tar -tvf "$arq" | cut -c1 | while IFS= read -r tipo; do
        case "$tipo" in
            - | d) ;;
            *)
                echo "FALHOU: o pacote tem uma entrada que não é arquivo nem pasta (tipo '$tipo'); recusado." >&2
                exit 1
                ;;
        esac
    done || exit 1

    mkdir "$tmp/x"
    tar -xf "$arq" -C "$tmp/x" --no-same-owner --no-overwrite-dir ||
        { echo "FALHOU: não consegui extrair o pacote." >&2; exit 1; }
    raiz=""
    n=0
    for d in "$tmp"/x/*; do
        if [ -d "$d" ]; then
            raiz=$d
            n=$((n + 1))
        fi
    done
    if [ "$n" -ne 1 ]; then
        echo "FALHOU: o pacote deveria ter uma só pasta de topo (pistick-<versão>/)." >&2
        exit 1
    fi
    for f in VERSAO PACOTES bin/psd libexec/iniciar-servico.sh libexec/primeiro-boot.sh \
        systemd/pistick.service.in systemd/pistick-primeiro-boot.service etc/psd.conf; do
        if [ ! -f "$raiz/$f" ]; then
            echo "FALHOU: o pacote não tem $f." >&2
            exit 1
        fi
    done
    versao=$(head -n 1 "$raiz/VERSAO" | tr -d '[:space:]')
}

if [ "$modo" = "desinstalar" ]; then
    exigir_root
    if [ "$sd" -eq 1 ] && systemctl is-active --quiet pistick 2>/dev/null; then
        systemctl stop pistick || falhou "serviço" "stop"
        estado "serviço" feito "parado"
    fi
    for u in pistick.service pistick-primeiro-boot.service; do
        if [ -f "/etc/systemd/system/$u" ] || [ -L "/etc/systemd/system/multi-user.target.wants/$u" ]; then
            systemctl disable "$u" >/dev/null 2>&1 || true
        fi
    done
    removido=0
    for f in "$unidade" "$unidade_pb" "$destino_psd"; do
        if [ -e "$f" ] || [ -L "$f" ]; then
            rm -f "$f"
            removido=1
        fi
    done
    for d in "$destino_libexec" "$destino_share"; do
        if [ -d "$d" ]; then
            rm -rf "$d"
            removido=1
        fi
    done
    rmdir /usr/local/libexec 2>/dev/null || true
    if [ "$sd" -eq 1 ]; then systemctl daemon-reload || true; fi
    if [ "$removido" -eq 1 ]; then
        estado "PiStick" feito "serviço e arquivos removidos"
    else
        estado "PiStick" ok "nada a remover"
    fi
    echo "Mantidos: $dir_conf, os pacotes apt e o usuário do serviço."
    exit 0
fi

if [ "$modo" = "verificar" ]; then
    raiz=""
    if [ -n "$pacote" ]; then
        obter_pacote
    fi
    if [ "$(uname -m)" = "aarch64" ]; then estado "arquitetura" ok "aarch64"; else falta "arquitetura" "$(uname -m), esperado aarch64"; fi
    so_ok=0
    if [ -r /etc/os-release ]; then
        so_id=$(. /etc/os-release && printf '%s' "${ID:-}")
        so_like=$(. /etc/os-release && printf '%s' "${ID_LIKE:-}")
        so_versao=$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")
        case " $so_id $so_like " in
            *" debian "* | *" raspbian "*)
                case "$so_versao" in 12 | 13) so_ok=1 ;; esac
                ;;
        esac
    fi
    if [ "$so_ok" -eq 1 ]; then estado "sistema" ok "$so_id $so_versao"; else falta "sistema" "esperado Debian 12 ou 13"; fi

    lista_pacotes=""
    if [ -n "$raiz" ]; then
        lista_pacotes=$raiz/PACOTES
    elif [ -f "$destino_share/PACOTES" ]; then
        lista_pacotes=$destino_share/PACOTES
    fi
    if [ -n "$lista_pacotes" ]; then
        faltam=""
        while IFS= read -r p; do
            case "$p" in '' | '#'*) continue ;; esac
            if ! dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q '^install ok installed'; then
                faltam="$faltam $p"
            fi
        done < "$lista_pacotes"
        if [ -z "$faltam" ]; then estado "pacotes apt" ok; else falta "pacotes apt" "faltam:$faltam"; fi
    else
        falta "pacotes apt" "sem a lista (PACOTES não instalado)"
    fi

    u=$usuario
    if [ -z "$u" ] && [ -f "$unidade" ]; then
        u=$(sed -n 's/^User=//p' "$unidade" | head -n 1)
    fi
    if [ -z "$u" ]; then
        falta "usuário do serviço" "sem unidade instalada"
    elif id "$u" >/dev/null 2>&1; then
        estado "usuário do serviço" ok "$u"
        for g in video render audio; do
            if getent group "$g" >/dev/null 2>&1; then
                if id -nG "$u" | tr ' ' '\n' | grep -qx "$g"; then estado "grupo $g" ok; else falta "grupo $g" "$u fora do grupo"; fi
            fi
        done
    else
        falta "usuário do serviço" "$u não existe"
    fi

    conferir_arquivo() {
        if [ ! -f "$2" ]; then
            falta "$1" "falta $2"
        elif [ -n "$3" ] && ! cmp -s "$3" "$2"; then
            falta "$1" "$2 difere do pacote"
        else
            estado "$1" ok "$2"
        fi
    }
    conferir_arquivo "psd" "$destino_psd" "${raiz:+$raiz/bin/psd}"
    conferir_arquivo "iniciar-servico.sh" "$destino_libexec/iniciar-servico.sh" "${raiz:+$raiz/libexec/iniciar-servico.sh}"
    conferir_arquivo "primeiro-boot.sh" "$destino_libexec/primeiro-boot.sh" "${raiz:+$raiz/libexec/primeiro-boot.sh}"
    conferir_arquivo "unidade pistick.service" "$unidade" ""
    conferir_arquivo "configuração" "$conf" ""
    if [ -d /etc/NetworkManager ] || command -v NetworkManager >/dev/null 2>&1; then
        conferir_arquivo "Wi-Fi sem economia de energia" /etc/NetworkManager/conf.d/pistick-wifi-sem-economia.conf "${raiz:+$raiz/etc/NetworkManager/pistick-wifi-sem-economia.conf}"
    fi
    conferir_arquivo "versão" "$destino_share/VERSAO" "${raiz:+$raiz/VERSAO}"
    if [ -f "$destino_share/VERSAO" ]; then echo "        instalada: $(head -n 1 "$destino_share/VERSAO")"; fi

    if systemctl is-enabled --quiet pistick 2>/dev/null; then estado "serviço habilitado" ok; else falta "serviço habilitado"; fi
    if [ "$sd" -eq 1 ]; then
        if systemctl is-active --quiet pistick 2>/dev/null; then estado "serviço ativo" ok; else falta "serviço ativo"; fi
    else
        estado "serviço ativo" ok "systemd não está rodando aqui; não conferido"
    fi
    if [ "$falhas" -ne 0 ]; then
        echo "Algo falta (veja as linhas FALTA acima)."
        exit 1
    fi
    exit 0
fi

exigir_root
checar_sistema
obter_pacote
estado "pacote" ok "versão $versao"

faltam=""
while IFS= read -r p; do
    case "$p" in '' | '#'*) continue ;; esac
    if ! dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q '^install ok installed'; then
        faltam="$faltam $p"
    fi
done < "$raiz/PACOTES"
if [ -z "$faltam" ]; then
    estado "pacotes apt" ok
else
    export DEBIAN_FRONTEND=noninteractive
    apt-get update || falhou "pacotes apt" "apt-get update"
    apt-get install -y --no-install-recommends $faltam || falhou "pacotes apt" "apt-get install"
    estado "pacotes apt" feito "instalados:$faltam"
fi

if [ -z "$usuario" ]; then
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        usuario=$SUDO_USER
    else
        usuario=pistick
        echo "aviso: sem SUDO_USER; usando o usuário próprio 'pistick'."
    fi
fi
case "$usuario" in
    root)
        echo "FALHOU: o serviço não roda como root; use --usuario NOME." >&2
        exit 1
        ;;
    *[!a-z0-9_-]* | -* | '')
        echo "FALHOU: nome de usuário inválido '$usuario'." >&2
        exit 1
        ;;
esac
if id "$usuario" >/dev/null 2>&1; then
    estado "usuário do serviço" ok "$usuario"
elif [ "$usuario" = "pistick" ]; then
    useradd --system --create-home --home-dir /home/pistick --shell /usr/sbin/nologin --user-group pistick ||
        falhou "usuário do serviço" "useradd pistick"
    estado "usuário do serviço" feito "pistick criado (sem senha, sem sudo, /home/pistick)"
else
    echo "FALHOU: o usuário '$usuario' não existe (use --usuario pistick para criar um usuário próprio)." >&2
    exit 1
fi
casa=$(getent passwd "$usuario" | cut -d: -f6)
if [ -z "$casa" ] || [ ! -d "$casa" ]; then
    echo "FALHOU: a pasta pessoal de '$usuario' ('$casa') não existe." >&2
    exit 1
fi
for g in video render audio; do
    if getent group "$g" >/dev/null 2>&1; then
        if id -nG "$usuario" | tr ' ' '\n' | grep -qx "$g"; then
            estado "grupo $g" ok "$usuario"
        else
            usermod -aG "$g" "$usuario" || falhou "grupo $g" "usermod"
            estado "grupo $g" feito "$usuario acrescentado"
        fi
    fi
done

mudou=0
instalar_arquivo() {
    mudou=0
    if [ -f "$3" ] && cmp -s "$2" "$3"; then
        estado "$1" ok
    else
        install -D -m "$4" "$2" "$3" || falhou "$1" "$3"
        mudou=1
        estado "$1" feito "$3"
    fi
}

instalar_arquivo "psd" "$raiz/bin/psd" "$destino_psd" 755
psd_mudou=$mudou
script_mudou=0
for f in "$raiz"/libexec/*; do
    instalar_arquivo "$(basename "$f")" "$f" "$destino_libexec/$(basename "$f")" 755
    if [ "$(basename "$f")" = "iniciar-servico.sh" ]; then script_mudou=$mudou; fi
done
shell_servico=$(getent passwd "$usuario" | cut -d: -f7)
case "${shell_servico:-}" in
    "" | */nologin | */false) shell_servico=/bin/bash ;;
    *) ;;
esac
sed -e "s|@USUARIO@|$usuario|g" -e "s|@CASA@|$casa|g" -e "s|@SHELL@|$shell_servico|g" "$raiz/systemd/pistick.service.in" > "$tmp/pistick.service"
instalar_arquivo "unidade pistick.service" "$tmp/pistick.service" "$unidade" 644
unidade_mudou=$mudou
if [ -f "$conf" ]; then
    estado "configuração" ok "$conf (mantida)"
else
    install -D -m 644 "$raiz/etc/psd.conf" "$conf" || falhou "configuração" "$conf"
    estado "configuração" feito "$conf"
fi
if [ -f "$raiz/etc/NetworkManager/pistick-wifi-sem-economia.conf" ]; then
    if [ -d /etc/NetworkManager ] || command -v NetworkManager >/dev/null 2>&1; then
        instalar_arquivo "Wi-Fi sem economia de energia" "$raiz/etc/NetworkManager/pistick-wifi-sem-economia.conf" /etc/NetworkManager/conf.d/pistick-wifi-sem-economia.conf 644
    else
        estado "Wi-Fi sem economia de energia" ok "NetworkManager não existe neste sistema; ignorado"
    fi
fi
instalar_arquivo "lista de pacotes" "$raiz/PACOTES" "$destino_share/PACOTES" 644
instalar_arquivo "versão" "$raiz/VERSAO" "$destino_share/VERSAO" 644

if [ "$sd" -eq 1 ] && [ "$unidade_mudou" -eq 1 ]; then
    systemctl daemon-reload || falhou "systemd" "daemon-reload"
fi
if systemctl is-enabled --quiet pistick 2>/dev/null; then
    estado "serviço habilitado" ok
else
    systemctl enable pistick.service >/dev/null 2>&1 || falhou "serviço habilitado" "systemctl enable"
    estado "serviço habilitado" feito
fi
if [ "$iniciar" -eq 0 ]; then
    estado "serviço" ok "não iniciado (--sem-iniciar)"
elif [ "$sd" -eq 0 ]; then
    estado "serviço" ok "systemd não está rodando aqui; inicia no próximo boot"
elif systemctl is-active --quiet pistick 2>/dev/null; then
    if [ "$psd_mudou" -eq 1 ] || [ "$script_mudou" -eq 1 ] || [ "$unidade_mudou" -eq 1 ]; then
        systemctl restart pistick || falhou "serviço" "restart"
        estado "serviço" feito "reiniciado: binário, script ou unidade mudou"
    else
        estado "serviço" ok "ativo, sem mudanças"
    fi
else
    systemctl restart pistick || falhou "serviço" "start"
    estado "serviço" feito "iniciado"
fi

echo
echo "PiStick $versao instalado (usuário do serviço: $usuario)."
echo "No computador: baixe o psc (cliente) e rode-o sem argumentos; ele procura o aparelho na rede."
echo "Os apps do serviço ficam em $conf; depois de editar: sudo systemctl restart pistick."
echo "Log do serviço: journalctl -u pistick -e"
