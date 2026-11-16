#!/bin/sh
# Roda NO PI (Raspberry Pi OS, aarch64): instala o Android (Waydroid) para o psd, de forma opcional e
# idempotente, no mesmo padrão do provisionar.sh. Cada passo confere antes de agir e imprime
# ok, feito, FALTA (só em --verificar) ou FALHOU. Veja docs/23-android-instalacao-e-uso.md.
#
#   sh tools/pi/instalar-android.sh              instala o que falta (baixa cerca de 1 GB na primeira vez)
#   sh tools/pi/instalar-android.sh --verificar  não muda nada; código 1 se algo falta
#   sh tools/pi/instalar-android.sh --remover    desfaz tudo: pacotes, imagem, dados, repositório, chave
#
# O que faz: repositório apt oficial do Waydroid (repo.waydro.id), o pacote `waydroid`, a imagem
# LineageOS VANILLA arm64 (`waydroid init`), serviços do LXC desabilitados, `waydroid-net.sh` com nftables,
# o `lmkd` (só se o kernel não tem PSI), a propriedade de várias janelas e o abrir-android.sh do psd.
# Não mexe em wlan0, NetworkManager, netplan, cmdline.txt, config.txt nem na rede do Pi.
set -eu

verificar=0
remover=0
for arg in "$@"; do
    case "$arg" in
        --verificar) verificar=1 ;;
        --remover) remover=1 ;;
        *)
            echo "uso: instalar-android.sh [--verificar | --remover]" >&2
            exit 2
            ;;
    esac
done
if [ "$verificar" -eq 1 ] && [ "$remover" -eq 1 ]; then
    echo "uso: instalar-android.sh [--verificar | --remover]" >&2
    exit 2
fi

if [ "$(id -u)" -eq 0 ]; then
    echo "FALHOU: não rode como root; o roteiro chama sudo onde precisa." >&2
    exit 1
fi
if [ "$(uname -m)" != "aarch64" ]; then
    echo "FALHOU: só roda em aarch64 (Raspberry Pi OS de 64 bits)." >&2
    exit 1
fi

repo=$(cd "$(dirname "$0")/../.." && pwd)
casa=$HOME
falhas=0
resumo=""
tmp=$(mktemp -d)

repo_url=https://repo.waydro.id
chave=/usr/share/keyrings/waydroid.gpg
lista_apt=/etc/apt/sources.list.d/waydroid.list
# shellcheck disable=SC1091
codinome=$(. /etc/os-release && echo "${VERSION_CODENAME:-trixie}")
net=/usr/lib/waydroid/data/scripts/waydroid-net.sh
net_original="$net.original-do-pacote"
dados=/var/lib/waydroid
overlay="$dados/overlay"
cfg="$dados/waydroid.cfg"
registro=/var/lib/pistick-android/pacotes-instalados.txt
libexec=/usr/local/libexec/pistick
policy=/usr/sbin/policy-rc.d

limpar() {
    rm -rf "$tmp" "$tmp.reg"
    # policy-rc.d nosso, se a execução foi interrompida durante o apt.
    if [ -f "$tmp.policy" ]; then
        sudo rm -f "$policy" "$tmp.policy" 2>/dev/null || true
    fi
}
trap limpar EXIT

# estado NOME RESULTADO [DETALHE]: imprime uma linha e guarda no resumo.
estado() {
    if [ -n "${3:-}" ]; then
        linha=$(printf '%-7s %s  (%s)' "$2" "$1" "$3")
    else
        linha=$(printf '%-7s %s' "$2" "$1")
    fi
    echo "$linha"
    resumo="$resumo
$linha"
}

falta() {
    estado "$1" FALTA "${2:-}"
    falhas=1
}

falhou() {
    estado "$1" FALHOU "${2:-}"
    echo "FALHOU: $1 ${2:-}" >&2
    exit 1
}

atualizado=0
apt_atualizado() {
    if [ "$atualizado" -eq 0 ]; then
        sudo apt-get update || falhou "apt" "apt-get update"
        atualizado=1
    fi
}

# pacotes ROTULO PACOTE...: instala só os que faltam.
pacotes() {
    rotulo=$1
    shift
    faltam=""
    for p in "$@"; do
        if ! dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q '^install ok installed'; then
            faltam="$faltam $p"
        fi
    done
    if [ -z "$faltam" ]; then
        estado "pacotes: $rotulo" ok
    elif [ "$verificar" -eq 1 ]; then
        falta "pacotes: $rotulo" "faltam:$faltam"
    else
        apt_atualizado
        # shellcheck disable=SC2086
        sudo apt-get install -y --no-install-recommends $faltam || falhou "pacotes: $rotulo" "apt-get install"
        estado "pacotes: $rotulo" feito "instalados:$faltam"
    fi
}

# instalar_arquivo ROTULO ORIGEM DESTINO MODO: copia com sudo só se o conteúdo mudou.
# Deixa mudou=1 se copiou (ou, em --verificar, se difere ou falta).
mudou=0
instalar_arquivo() {
    mudou=0
    if sudo test -f "$3" && sudo cmp -s "$2" "$3"; then
        estado "$1" ok
    elif [ "$verificar" -eq 1 ]; then
        mudou=1
        falta "$1" "$3"
    else
        sudo install -D -m "$4" "$2" "$3" || falhou "$1" "$3"
        mudou=1
        estado "$1" feito "$3"
    fi
}

# remover_arquivo ROTULO CAMINHO: apaga (com sudo) se existe.
remover_arquivo() {
    if sudo test -e "$2" || sudo test -L "$2"; then
        sudo rm -rf "$2" || falhou "$1" "rm $2"
        estado "$1" feito "removido: $2"
    else
        estado "$1" ok "já não existe"
    fi
}

# ---------------------------------------------------------------------------------------------
# Remoção
# ---------------------------------------------------------------------------------------------
if [ "$remover" -eq 1 ]; then
    # 1. O contêiner e a ponte. O D-Bus ativaria o serviço de novo se alguém rodasse `waydroid status`.
    sudo systemctl stop waydroid-container 2>/dev/null || true
    if ip link show waydroid0 >/dev/null 2>&1; then
        sudo ip link del waydroid0 2>/dev/null || true
    fi
    if ip link show waydroid0 >/dev/null 2>&1; then
        falhou "ponte waydroid0" "não foi possível apagar"
    fi
    estado "contêiner e ponte" ok "parados"

    # Nada pode estar montado dentro dos dados do Waydroid antes do rm -rf.
    if findmnt -rn -o TARGET | grep -q "^$dados"; then
        falhou "montagens" "ainda há montagens em $dados (o contêiner não parou?); nada foi apagado"
    fi

    # 2. Desvio do waydroid-net.sh (antes de remover o pacote).
    if dpkg-divert --list "$net" 2>/dev/null | grep -q .; then
        sudo rm -f "$net"
        sudo dpkg-divert --local --rename --remove "$net" >/dev/null || falhou "desvio de waydroid-net.sh" "dpkg-divert --remove"
        estado "desvio de waydroid-net.sh" feito "removido"
    else
        estado "desvio de waydroid-net.sh" ok "não havia"
    fi

    # 3. Pacotes: os que a instalação registrou; sem registro (instalado à mão), o waydroid e o que o apt
    # passar a considerar órfão depois dele.
    if [ -f "$registro" ]; then
        lista_pacotes=$(grep -v '^[[:space:]]*$' "$registro" | tr '\n' ' ')
        instalados=""
        for p in $lista_pacotes; do
            if dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'ok'; then
                instalados="$instalados $p"
            fi
        done
        if [ -z "$instalados" ]; then
            estado "pacotes do Android" ok "nenhum instalado"
        else
            # Conferência: o apt só pode remover o que a instalação registrou.
            echo "$lista_pacotes" | tr ' ' '\n' | grep -v '^$' | sort -u > "$tmp.reg"
            # shellcheck disable=SC2086
            a_remover=$(sudo apt-get -s purge $instalados | awk '/^Purg /{print $2}' | sort -u)
            fora=$(printf '%s\n' "$a_remover" | grep -vxF -f "$tmp.reg" || true)
            if [ -n "$fora" ]; then
                falhou "pacotes do Android" "o apt também removeria pacotes que a instalação não registrou:$(echo "$fora" | tr '\n' ' ')"
            fi
            # shellcheck disable=SC2086
            sudo apt-get purge -y $instalados || falhou "pacotes do Android" "apt-get purge"
            estado "pacotes do Android" feito "removidos: $(echo "$instalados" | wc -w) pacotes"
        fi
    elif dpkg-query -W -f='${Status}' waydroid 2>/dev/null | grep -q 'ok'; then
        sudo apt-get purge -y waydroid || falhou "pacotes do Android" "apt-get purge waydroid"
        sudo apt-get autoremove --purge -y || falhou "pacotes do Android" "apt-get autoremove"
        estado "pacotes do Android" feito "waydroid e órfãos (sem registro da instalação)"
    else
        estado "pacotes do Android" ok "waydroid não está instalado"
    fi

    # 4. Dados, camada overlay, estado.
    remover_arquivo "dados do Waydroid ($dados)" "$dados"
    remover_arquivo "dados do usuário (~/.local/share/waydroid)" "$casa/.local/share/waydroid"
    for f in "$casa"/.local/share/applications/waydroid.*.desktop "$casa/.local/share/applications/Waydroid.desktop"; do
        if [ -e "$f" ]; then rm -f "$f"; estado "atalho $(basename "$f")" feito "removido"; fi
    done
    remover_arquivo "restos do dnsmasq da ponte" /var/lib/misc/dnsmasq.waydroid0.leases
    remover_arquivo "script abrir-android.sh instalado" "$libexec/abrir-android.sh"
    remover_arquivo "script parar-android.sh instalado" "$libexec/parar-android.sh"
    remover_arquivo "registro de pacotes" "$registro"
    sudo rmdir "$(dirname "$registro")" 2>/dev/null || true
    # 5. Repositório e chave.
    remover_arquivo "repositório apt" "$lista_apt"
    remover_arquivo "chave do repositório" "$chave"
    sudo rm -f /var/lib/apt/lists/repo.waydro.id_* 2>/dev/null || true

    echo
    echo "== Resumo =="
    echo "$resumo" | sed '1{/^$/d}'
    echo
    echo "Se o /etc/pistick/psd.conf chama abrir-android.sh em algum APP_n, tire a linha (o serviço não sobe esse app sem o Android)."
    exit 0
fi

# ---------------------------------------------------------------------------------------------
# Instalação e verificação
# ---------------------------------------------------------------------------------------------

# 1. Repositório apt do projeto Waydroid: chave em /usr/share/keyrings e lista em sources.list.d.
if [ -s "$chave" ]; then
    estado "chave do repositório" ok "$chave"
elif [ "$verificar" -eq 1 ]; then
    falta "chave do repositório" "falta $chave"
else
    command -v curl >/dev/null 2>&1 || falhou "chave do repositório" "curl não instalado (sh tools/pi/provisionar.sh)"
    curl --proto '=https' --tlsv1.2 -fsS "$repo_url/waydroid.gpg" -o "$tmp/waydroid.gpg" || falhou "chave do repositório" "download de $repo_url/waydroid.gpg"
    [ -s "$tmp/waydroid.gpg" ] || falhou "chave do repositório" "arquivo vazio"
    sudo install -D -m 644 "$tmp/waydroid.gpg" "$chave" || falhou "chave do repositório" "$chave"
    estado "chave do repositório" feito "$chave"
fi
echo "deb [signed-by=$chave] $repo_url/ $codinome main" > "$tmp/waydroid.list"
instalar_arquivo "repositório apt" "$tmp/waydroid.list" "$lista_apt" 644
if [ "$mudou" -eq 1 ] && [ "$verificar" -eq 0 ]; then
    apt_atualizado
fi

# 2. Pacote do Waydroid. Antes, registra o que o apt vai instalar (a remoção usa essa lista). Durante a
# instalação um policy-rc.d temporário impede os pós-instalação de iniciar serviços: o `lxc` sobe o
# `lxc-net` (ponte lxcbr0, tabelas nft, dnsmasq) e a rede do Pi não deve mudar.
if dpkg-query -W -f='${Status}' waydroid 2>/dev/null | grep -q '^install ok installed'; then
    estado "pacote waydroid" ok "$(dpkg-query -W -f='${Version}' waydroid)"
elif [ "$verificar" -eq 1 ]; then
    falta "pacote waydroid"
else
    apt_atualizado
    sudo apt-get -s install --no-install-recommends waydroid | awk '/^Inst /{print $2}' > "$tmp/novos.txt" || true
    [ -s "$tmp/novos.txt" ] || falhou "pacote waydroid" "o apt não listou nada a instalar (repositório sem o pacote?)"
    sudo install -D -m 644 "$tmp/novos.txt" "$registro" || falhou "pacote waydroid" "$registro"
    if [ -e "$policy" ]; then
        estado "policy-rc.d" ok "já existe um; usado como está"
        usa_policy=0
    else
        printf '#!/bin/sh\nexit 101\n' > "$tmp.policy"
        sudo install -m 755 "$tmp.policy" "$policy" || falhou "policy-rc.d" "$policy"
        usa_policy=1
    fi
    if sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends waydroid; then
        if [ "$usa_policy" -eq 1 ]; then sudo rm -f "$policy" "$tmp.policy"; fi
    else
        if [ "$usa_policy" -eq 1 ]; then sudo rm -f "$policy" "$tmp.policy"; fi
        falhou "pacote waydroid" "apt-get install"
    fi
    estado "pacote waydroid" feito "$(wc -l < "$tmp/novos.txt") pacotes; lista em $registro"
fi
if [ -e "$policy" ] && [ "$verificar" -eq 1 ]; then
    falta "policy-rc.d temporário" "sobrou $policy de uma instalação interrompida"
fi

# 3. Serviços: o contêiner só roda quando o Android é pedido (o D-Bus ativa o waydroid-container na
# hora), e as unidades do LXC (ponte lxcbr0, tabela nft própria) conflitam com a ponte do Waydroid.
# Unidades `static` (lxc-monitord) não têm o que desabilitar.
for u in waydroid-container lxc-net lxc lxc-monitord lxcfs; do
    if ! systemctl cat "$u.service" >/dev/null 2>&1; then
        estado "serviço $u" ok "não existe"
        continue
    fi
    st=$(systemctl is-enabled "$u.service" 2>/dev/null || true)
    case "$st" in
        enabled | enabled-runtime | alias | linked | linked-runtime)
            if [ "$verificar" -eq 1 ]; then
                falta "serviço $u desabilitado" "está $st"
            else
                sudo systemctl disable --now "$u.service" || falhou "serviço $u" "systemctl disable --now"
                estado "serviço $u desabilitado" feito "estava $st"
            fi
            ;;
        *) estado "serviço $u desabilitado" ok "${st:-desconhecido}" ;;
    esac
done

# 4. waydroid-net.sh: o kernel do Pi não tem ip_tables legado, então a ponte precisa de nftables
# (LXC_USE_NFT="true"). O arquivo é do pacote e um `apt upgrade` o desfaria. Solução: `dpkg-divert`.
# O arquivo original vai para "$net_original" (um upgrade do pacote grava a versão nova lá) e no lugar
# fica uma cópia gerada a partir dele só com essa linha trocada; reaplicada a cada execução.
if [ -f "$net" ] || [ -f "$net_original" ]; then
    desviado=0
    if dpkg-divert --list "$net" 2>/dev/null | grep -q .; then desviado=1; fi
    if [ "$desviado" -eq 1 ]; then base=$net_original; else base=$net; fi
    if grep -q '^LXC_USE_NFT="true"' "$base"; then
        estado "waydroid-net.sh com nftables" ok "o pacote já usa nft"
    else
        grep -q '^LXC_USE_NFT="false"$' "$base" || falhou "waydroid-net.sh" "não achei a linha LXC_USE_NFT=\"false\" em $base (o pacote mudou?)"
        sed 's/^LXC_USE_NFT="false"$/LXC_USE_NFT="true"/' "$base" > "$tmp/net.sh"
        if [ "$desviado" -eq 1 ] && sudo cmp -s "$net" "$tmp/net.sh"; then
            estado "waydroid-net.sh com nftables" ok "desvio do dpkg + cópia nossa"
        elif [ "$verificar" -eq 1 ]; then
            falta "waydroid-net.sh com nftables" "falta o desvio e a cópia com LXC_USE_NFT=\"true\""
        else
            if [ "$desviado" -eq 0 ]; then
                sudo dpkg-divert --local --rename --divert "$net_original" --add "$net" >/dev/null || falhou "waydroid-net.sh" "dpkg-divert --add"
            fi
            sudo install -m 755 "$tmp/net.sh" "$net" || falhou "waydroid-net.sh" "$net"
            estado "waydroid-net.sh com nftables" feito "desvio do dpkg; original em $net_original"
        fi
    fi
elif [ "$verificar" -eq 1 ]; then
    falta "waydroid-net.sh" "o pacote não está instalado"
fi

# 5. Imagem do Android (LineageOS, variante VANILLA, sem os apps do Google). Só baixa se não existe.
if sudo test -s "$dados/images/system.img" && sudo test -s "$dados/images/vendor.img" && [ -f "$cfg" ]; then
    estado "imagem do Android" ok "$dados/images"
elif [ "$verificar" -eq 1 ]; then
    falta "imagem do Android" "falta $dados/images (waydroid init -s VANILLA)"
elif ! command -v waydroid >/dev/null 2>&1; then
    falhou "imagem do Android" "waydroid não está instalado"
else
    sudo waydroid init -s VANILLA || falhou "imagem do Android" "waydroid init -s VANILLA"
    estado "imagem do Android" feito "waydroid init -s VANILLA"
fi

# 6. lmkd. Com PSI (/proc/pressure/memory) o lmkd do próprio Android funciona e NENHUM substituto é
# instalado (e o que houver na camada overlay sai). Sem PSI, instala o substituto nosso e o lmkd.rc sem
# `critical` (veja tools/pi/android/lmkd-substituto.c).
lmkd_bin="$overlay/system/bin/lmkd"
lmkd_rc="$overlay/system/etc/init/lmkd.rc"
if [ -e /proc/pressure/memory ]; then
    if sudo test -e "$lmkd_bin" || sudo test -e "$lmkd_rc"; then
        if [ "$verificar" -eq 1 ]; then
            falta "lmkd do Android (kernel com PSI)" "sobrou o substituto na camada overlay"
        else
            sudo rm -f "$lmkd_bin" "$lmkd_rc"
            estado "lmkd do Android (kernel com PSI)" feito "substituto removido da camada overlay"
        fi
    else
        estado "lmkd: o do Android (kernel com PSI)" ok "sem substituto"
    fi
else
    if ! command -v gcc >/dev/null 2>&1 || ! [ -f /usr/lib/aarch64-linux-gnu/libc.a ]; then
        pacotes "compilação do lmkd" gcc libc6-dev
    fi
    if command -v gcc >/dev/null 2>&1 && [ -f /usr/lib/aarch64-linux-gnu/libc.a ]; then
        gcc -O2 -s -static -Wall -Wextra -o "$tmp/lmkd" "$repo/tools/pi/android/lmkd-substituto.c" || falhou "lmkd substituto" "gcc"
        instalar_arquivo "lmkd: substituto nosso (kernel sem PSI): binário" "$tmp/lmkd" "$lmkd_bin" 755
        # O init do Android só aceita fim de linha LF (o git archive do sync-pi converte para CRLF sem o .gitattributes).
        tr -d '\r' < "$repo/tools/pi/android/lmkd.rc" > "$tmp/lmkd.rc"
        instalar_arquivo "lmkd: substituto nosso (kernel sem PSI): lmkd.rc sem critical" "$tmp/lmkd.rc" "$lmkd_rc" 644
    fi
fi

# 7. Propriedade do Android: cada app em sua janela (persist.waydroid.multi_windows=true), na seção
# [properties] do waydroid.cfg (o Waydroid a copia para o waydroid.prop a cada partida do contêiner).
if [ -f "$cfg" ]; then
    if python3 -c '
import configparser, sys
c = configparser.ConfigParser()
c.read(sys.argv[1])
sys.exit(0 if c.get("properties", "persist.waydroid.multi_windows", fallback="") == "true" else 1)
' "$cfg"; then
        estado "propriedade multi_windows=true" ok
    elif [ "$verificar" -eq 1 ]; then
        falta "propriedade multi_windows=true" "em $cfg"
    else
        sudo python3 -c '
import configparser, sys
p = sys.argv[1]
c = configparser.ConfigParser()
c.read(p)
if "properties" not in c:
    c["properties"] = {}
c["properties"]["persist.waydroid.multi_windows"] = "true"
with open(p, "w") as f:
    c.write(f)
' "$cfg" || falhou "propriedade multi_windows" "$cfg"
        estado "propriedade multi_windows=true" feito "$cfg (vale na próxima partida do contêiner)"
    fi
else
    if [ "$verificar" -eq 1 ]; then falta "propriedade multi_windows=true" "falta $cfg"; fi
fi

# 8. Script que o psd usa para abrir o Android (APP_n=/usr/local/libexec/pistick/abrir-android.sh [PACOTE]).
instalar_arquivo "abrir-android.sh" "$repo/tools/pi/android/abrir-android.sh" "$libexec/abrir-android.sh" 755
# O pistick.service o chama (como root) no ExecStopPost: espera o contêiner parar e então para o waydroid-container.
instalar_arquivo "parar-android.sh" "$repo/tools/pi/android/parar-android.sh" "$libexec/parar-android.sh" 755

# 9. Estado do contêiner: parado e desabilitado quando ninguém pediu o Android.
if pgrep -x lxc-start >/dev/null 2>&1; then
    estado "contêiner" ok "rodando agora (o Android está em uso)"
elif systemctl is-active --quiet waydroid-container 2>/dev/null; then
    estado "contêiner" ok "parado (o daemon waydroid-container, ocioso, ainda está ativo; para: sudo systemctl stop waydroid-container)"
else
    estado "contêiner" ok "parado"
fi

echo
echo "== Resumo =="
echo "$resumo" | sed '1{/^$/d}'
echo
if [ -e /proc/pressure/memory ]; then
    echo "lmkd: o do Android (kernel com PSI)."
else
    echo "lmkd: substituto nosso (kernel sem /proc/pressure/memory)."
fi
if [ "$falhas" -ne 0 ]; then
    echo "Algo falta (veja as linhas FALTA acima)."
    exit 1
fi
