#!/bin/sh
# Constrói a imagem pronta para gravar (pistick-<versão>-rpi-arm64.img.xz) a partir da imagem oficial
# do Raspberry Pi OS Lite arm64 e do pacote do PiStick. Roda NO PI (aarch64 nativo), com sudo:
#
#   sudo sh tools/imagem/montar-imagem.sh --pacote ~/pistick-saida/pistick-<versão>-aarch64.tar.xz [--saida PASTA]
#
# Veja docs/17-imagem-e-instalador.md. Passos: baixa e confere a base (cache/), descompacta, cresce o
# arquivo e a partição raiz, monta por dispositivo de laço, aplica o instalador por chroot, limpa,
# desmonta, zera o espaço livre e comprime.
#
# SEGURANÇA: este roteiro só escreve em ARQUIVOS e em dispositivos de laço que ele mesmo cria a
# partir do arquivo de trabalho. Recusa caminho de dispositivo de bloco (argumentos e arquivos) e
# confere, antes de cada comando destrutivo, que o alvo é um laço cujo arquivo de apoio é o de
# trabalho. Nunca aponta para o cartão do Pi nem para qualquer disco.
#
# Um trap desmonta tudo e solta o laço em qualquer saída (erro, Ctrl+C, SIGTERM). Reexecuta-se sob
# nice -n 19 e ionice -c3: o Pi pode estar em uso por outro trabalho.
set -eu

if [ -z "${PISTICK_NICE:-}" ]; then
    PISTICK_NICE=1
    export PISTICK_NICE
    if command -v ionice >/dev/null 2>&1; then
        exec nice -n 19 ionice -c3 sh "$0" "$@"
    fi
    exec nice -n 19 sh "$0" "$@"
fi

uso() {
    echo "uso: sudo sh montar-imagem.sh --pacote ARQUIVO.tar.xz [--saida PASTA] [--manter]" >&2
    echo "     --manter: não apaga o arquivo .img de trabalho (descompactado) ao final" >&2
}

pacote=""
saida=""
manter=0
while [ $# -gt 0 ]; do
    case "$1" in
        --pacote)
            [ $# -ge 2 ] || { uso; exit 2; }
            pacote=$2
            shift 2
            ;;
        --saida)
            [ $# -ge 2 ] || { uso; exit 2; }
            saida=$2
            shift 2
            ;;
        --manter)
            manter=1
            shift
            ;;
        *)
            uso
            exit 2
            ;;
    esac
done
[ -n "$pacote" ] || { uso; exit 2; }

falhou() {
    echo "FALHOU: $*" >&2
    exit 1
}

# recusar_bloco ROTULO CAMINHO: recusa dispositivo de bloco, qualquer caminho em /dev, /proc, /sys.
recusar_bloco() {
    case "$2" in
        /dev | /dev/* | /proc | /proc/* | /sys | /sys/*)
            falhou "$1: '$2' é um caminho de dispositivo; este roteiro só trabalha com arquivos."
            ;;
    esac
    real=$(readlink -f "$2" 2>/dev/null || printf '%s' "$2")
    if [ -b "$2" ] || [ -b "$real" ]; then
        falhou "$1: '$2' é um dispositivo de bloco; este roteiro só trabalha com arquivos de imagem."
    fi
    case "$real" in
        /dev | /dev/* | /proc | /proc/* | /sys | /sys/*)
            falhou "$1: '$2' resolve para '$real', um caminho de dispositivo."
            ;;
    esac
}

[ "$(id -u)" -eq 0 ] || falhou "rode com sudo."
[ "$(uname -m)" = "aarch64" ] || falhou "só roda em aarch64 (o Pi), sem emulação."
for c in losetup parted e2fsck resize2fs zerofree xz mount umount chroot sha256sum curl truncate tar mountpoint; do
    command -v "$c" >/dev/null 2>&1 || falhou "falta o comando '$c' (sh tools/pi/provisionar.sh --dev instala o que for de pacote)."
done

dono=""
if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
    dono=$SUDO_USER
fi
if [ -z "$saida" ]; then
    if [ -n "$dono" ]; then
        saida="$(getent passwd "$dono" | cut -d: -f6)/pistick-saida"
    else
        saida="$HOME/pistick-saida"
    fi
fi

recusar_bloco "--pacote" "$pacote"
recusar_bloco "--saida" "$saida"
[ -f "$pacote" ] || falhou "o pacote '$pacote' não existe."
mkdir -p "$saida/cache" "$saida/trabalho"
saida=$(cd "$saida" && pwd)
pacote=$(cd "$(dirname "$pacote")" && pwd)/$(basename "$pacote")

aqui=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=base.conf
. "$aqui/base.conf"

inicio=$(date +%s)
sucesso=0
laco=""
mnt=""
img=""
lock="$saida/trabalho/.lock"
resolv_orig=""
tmp=""

# Um só construtor por pasta de saída.
if ! mkdir "$lock" 2>/dev/null; then
    outro=$(cat "$lock/pid" 2>/dev/null || true)
    if [ -n "$outro" ] && kill -0 "$outro" 2>/dev/null; then
        falhou "já há um construtor rodando (pid $outro) em $saida."
    fi
    rm -rf "$lock"
    mkdir "$lock"
fi
echo $$ > "$lock/pid"

# matar_chroot: encerra o que roda com a raiz em $mnt (processos do chroot que sobraram).
matar_chroot() {
    for sinal in TERM KILL; do
        achou=0
        for p in /proc/[0-9]*; do
            r=$(readlink "$p/root" 2>/dev/null || true)
            if [ "$r" = "$mnt" ]; then
                kill -"$sinal" "${p#/proc/}" 2>/dev/null || true
                achou=1
            fi
        done
        if [ "$achou" -eq 1 ]; then sleep 1; fi
    done
}

# Limpeza em qualquer saída: mata o que roda no chroot, desmonta em ordem inversa, solta o laço.
limpar() {
    set +e
    trap '' INT TERM HUP
    if [ -n "$mnt" ]; then
        matar_chroot
        for m in "$mnt/dev/pts" "$mnt/dev" "$mnt/sys" "$mnt/proc" "$mnt/boot/firmware" "$mnt"; do
            tentativas=0
            while mountpoint -q "$m" 2>/dev/null; do
                if umount "$m" 2>/dev/null; then break; fi
                tentativas=$((tentativas + 1))
                if [ "$tentativas" -ge 5 ]; then
                    umount -l "$m" 2>/dev/null
                    break
                fi
                sleep 1
            done
        done
        rmdir "$mnt" 2>/dev/null
    fi
    if [ -n "$laco" ]; then
        losetup -d "$laco" 2>/dev/null
    fi
    if [ "$sucesso" -ne 1 ] && [ -n "$img" ]; then
        rm -f "$img" "$img.xz.parcial"
        echo "Construção interrompida: nada ficou montado; o arquivo de trabalho foi apagado." >&2
    elif [ "$manter" -eq 0 ] && [ -n "$img" ]; then
        rm -f "$img"
    fi
    if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
    rm -rf "$lock"
}
trap limpar EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# confere_laco: o alvo é um laço /dev/loopN cujo arquivo de apoio é exatamente o arquivo de trabalho.
confere_laco() {
    case "$laco" in
        /dev/loop[0-9] | /dev/loop[0-9][0-9] | /dev/loop[0-9][0-9][0-9]) ;;
        *) falhou "alvo '$laco' não é um dispositivo de laço." ;;
    esac
    apoio=$(losetup -nO BACK-FILE "$laco" 2>/dev/null || true)
    [ "$apoio" = "$img" ] || falhou "o laço '$laco' não aponta para o arquivo de trabalho ('$apoio' != '$img')."
}

# alvo_particao N: imprime ${laco}pN depois de conferir o laço e esperar o nó aparecer.
alvo_particao() {
    confere_laco
    n=0
    while [ ! -b "${laco}p$1" ]; do
        n=$((n + 1))
        [ "$n" -le 50 ] || falhou "o nó ${laco}p$1 não apareceu."
        sleep 0.2
    done
    printf '%s' "${laco}p$1"
}

passo() {
    echo
    echo "== $* =="
}

# ---- 0. Pacote ----
passo "Pacote"
tmp=$(mktemp -d)
tar -xJf "$pacote" -C "$tmp" --no-same-owner || falhou "não consegui extrair o pacote."
raiz=""
for d in "$tmp"/pistick-*; do
    [ -d "$d" ] && raiz=$d
done
[ -n "$raiz" ] || falhou "o pacote não tem a pasta pistick-<versão>/."
for f in VERSAO instalar-local.sh systemd/pistick-primeiro-boot.service; do
    [ -f "$raiz/$f" ] || falhou "o pacote não tem $f."
done
versao=$(head -n 1 "$raiz/VERSAO" | tr -d '[:space:]')
echo "versão do pacote: $versao"
img="$saida/trabalho/pistick-$versao-rpi-arm64.img"
final="$saida/pistick-$versao-rpi-arm64.img.xz"
recusar_bloco "arquivo de trabalho" "$img"
recusar_bloco "imagem final" "$final"
rm -f "$img"
cp "$pacote" "$tmp/pacote.tar.xz"
if [ -f "$pacote.sha256" ]; then cp "$pacote.sha256" "$tmp/pacote.tar.xz.sha256"; fi

# ---- 1. Imagem base ----
passo "Imagem base $BASE_VERSAO"
base="$saida/cache/$BASE_ARQUIVO"
if [ ! -f "$base" ]; then
    echo "baixando $BASE_URL"
    curl --proto '=https' --tlsv1.2 -fSL -o "$base.parcial" "$BASE_URL" || { rm -f "$base.parcial"; falhou "download da base."; }
    mv "$base.parcial" "$base"
fi
real=$(sha256sum "$base" | cut -d' ' -f1)
if [ "$real" != "$BASE_SHA256" ]; then
    rm -f "$base"
    falhou "o SHA-256 da base ($real) não é o fixado em base.conf; arquivo apagado."
fi
echo "SHA-256 da base conferido: $BASE_SHA256"

# ---- 2. Arquivo de trabalho e partição raiz ----
passo "Arquivo de trabalho"
xz -dc "$base" > "$img" || falhou "não consegui descompactar a base."
tamanho_base=$(wc -c < "$img")
truncate -s "+${CRESCER_MIB}M" "$img"
parted -s "$img" resizepart 2 100% || falhou "parted resizepart"
parted -s "$img" unit B print | sed -n '/^Number/,$p'

# ---- 3. Laço e montagem ----
passo "Laço e montagem"
laco=$(losetup --partscan --find --show "$img") || falhou "losetup"
echo "laço: $laco -> $img"
part1=$(alvo_particao 1)
part2=$(alvo_particao 2)
confere_laco
e2fsck -f -p "$part2" || [ $? -le 1 ] || falhou "e2fsck da raiz ($part2)."
confere_laco
resize2fs "$part2" || falhou "resize2fs"

mnt=$(mktemp -d /var/tmp/pistick-img.XXXXXX)
mount "$part2" "$mnt" || falhou "mount da raiz"
mkdir -p "$mnt/boot/firmware"
mount "$part1" "$mnt/boot/firmware" || falhou "mount do boot"
mount -t proc proc "$mnt/proc"
mount -t sysfs sysfs "$mnt/sys"
mount --bind /dev "$mnt/dev"
mount --bind /dev/pts "$mnt/dev/pts"

# Estado da base que não pode mudar.
nm_antes=$(find "$mnt/etc/NetworkManager/system-connections" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort | tr '\n' ' ')
np_antes=$(find "$mnt/etc/netplan" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort | tr '\n' ' ')
mid_antes=$(cat "$mnt/etc/machine-id" 2>/dev/null || true)

# resolv.conf provisório (o original volta depois) e política de não iniciar serviços no chroot.
resolv_orig="$tmp/resolv.conf.orig"
if [ -e "$mnt/etc/resolv.conf" ] || [ -L "$mnt/etc/resolv.conf" ]; then
    cp -a "$mnt/etc/resolv.conf" "$resolv_orig"
fi
rm -f "$mnt/etc/resolv.conf"
cp /etc/resolv.conf "$mnt/etc/resolv.conf"
printf '#!/bin/sh\nexit 101\n' > "$mnt/usr/sbin/policy-rc.d"
chmod 755 "$mnt/usr/sbin/policy-rc.d"

# ---- 4. Instalação no chroot ----
passo "Instalação no chroot"
cp "$tmp/pacote.tar.xz" "$mnt/tmp/pacote.tar.xz"
if [ -f "$tmp/pacote.tar.xz.sha256" ]; then cp "$tmp/pacote.tar.xz.sha256" "$mnt/tmp/pacote.tar.xz.sha256"; fi
cp "$raiz/instalar-local.sh" "$mnt/tmp/instalar-local.sh"
cp "$raiz/systemd/pistick-primeiro-boot.service" "$mnt/tmp/pistick-primeiro-boot.service"
cat > "$mnt/tmp/pistick-chroot.sh" <<CHROOT
#!/bin/sh
set -eu
export DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 LC_ALL=C.UTF-8
apt-get update
if [ "$REMOVER_COMPILADOR" = "1" ]; then
    # A base oficial traz o compilador; a imagem do PiStick não deve ter.
    pacotes_cc=\$(dpkg-query -W -f='\${Package} \${Status}\n' | grep ' install ok installed\$' | cut -d' ' -f1 |
        grep -E '^(build-essential|gcc|g\+\+|cpp)(-[0-9]+)?(-aarch64-linux-gnu)?\$|^(gcc|g\+\+|cpp)-[0-9]+-(aarch64-linux-gnu|for-host)\$' || true)
    if [ -n "\$pacotes_cc" ]; then
        echo "removendo o compilador da base:" \$pacotes_cc
        apt-get purge -y \$pacotes_cc
    fi
fi
sh /tmp/instalar-local.sh --pacote /tmp/pacote.tar.xz --usuario pistick --sem-iniciar
install -D -m 644 /tmp/pistick-primeiro-boot.service /etc/systemd/system/pistick-primeiro-boot.service
systemctl enable pistick-primeiro-boot.service
echo pistick > /etc/hostname
sed -i 's/^\(127\.0\.1\.1[[:space:]]\{1,\}\).*/\1pistick/' /etc/hosts
grep -q '^127\.0\.1\.1' /etc/hosts || printf '127.0.1.1\tpistick\n' >> /etc/hosts
# Limpeza.
apt-get clean
rm -rf /var/lib/apt/lists/*
find /var/log -type f -exec rm -f {} +
rm -f /root/.bash_history /root/.wget-hsts /root/.lesshst /root/.viminfo
rm -rf /root/.cache
for h in /home/*; do
    [ -d "\$h" ] || continue
    rm -f "\$h/.bash_history" "\$h/.lesshst" "\$h/.viminfo"
    rm -rf "\$h/.cache"
done
CHROOT
chmod 755 "$mnt/tmp/pistick-chroot.sh"
# Em segundo plano e com wait: o trap de INT/TERM roda na hora, sem esperar o chroot terminar.
chroot "$mnt" /bin/sh /tmp/pistick-chroot.sh &
pid_chroot=$!
wait "$pid_chroot" || falhou "a instalação no chroot falhou."

# ---- 5. Limpeza e conferência da base ----
passo "Limpeza"
find "$mnt/tmp" "$mnt/var/tmp" -mindepth 1 -xdev -delete
rm -f "$mnt/usr/sbin/policy-rc.d"
rm -f "$mnt/etc/resolv.conf"
if [ -e "$resolv_orig" ] || [ -L "$resolv_orig" ]; then
    cp -a "$resolv_orig" "$mnt/etc/resolv.conf"
fi
mid_depois=$(cat "$mnt/etc/machine-id" 2>/dev/null || true)
if [ "$mid_depois" != "$mid_antes" ]; then
    printf '%s\n' "$mid_antes" > "$mnt/etc/machine-id"
    echo "machine-id restaurado ao da base"
fi
rm -f "$mnt/var/lib/dbus/machine-id.novo"
for f in "$mnt"/etc/ssh/ssh_host_*; do
    [ -e "$f" ] && falhou "sobrou chave SSH de máquina: ${f#"$mnt"}"
done
nm_depois=$(find "$mnt/etc/NetworkManager/system-connections" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort | tr '\n' ' ')
np_depois=$(find "$mnt/etc/netplan" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort | tr '\n' ' ')
[ "$nm_antes" = "$nm_depois" ] || falhou "perfis de rede novos em /etc/NetworkManager/system-connections."
[ "$np_antes" = "$np_depois" ] || falhou "arquivos novos em /etc/netplan."
echo "sem chaves SSH de máquina, sem perfis de rede novos, machine-id como na base"

livre_kib=$(df -Pk "$mnt" | awk 'NR == 2 { print $4 }')
usado_kib=$(df -Pk "$mnt" | awk 'NR == 2 { print $3 }')
echo "raiz: ${usado_kib} KiB usados, ${livre_kib} KiB livres"
if [ "$livre_kib" -lt 204800 ]; then
    falhou "sobrou menos de 200 MiB livres na raiz; aumente CRESCER_MIB em base.conf."
fi

# ---- 6. Desmontar, zerar o espaço livre, soltar o laço ----
passo "Desmontagem e zeragem do espaço livre"
sync
matar_chroot
for m in "$mnt/dev/pts" "$mnt/dev" "$mnt/sys" "$mnt/proc" "$mnt/boot/firmware" "$mnt"; do
    umount "$m" || falhou "não consegui desmontar $m."
done
rmdir "$mnt"
mnt=""
part2=$(alvo_particao 2)
e2fsck -f -n "$part2" > /dev/null || falhou "e2fsck -n depois da instalação."
confere_laco
zerofree "$part2" || falhou "zerofree"
confere_laco
losetup -d "$laco" || falhou "losetup -d"
laco=""

# ---- 7. Compressão ----
passo "Compressão (xz -T0 -6)"
xz -T0 -6 -c "$img" > "$img.xz.parcial" || falhou "xz"
mv -f "$img.xz.parcial" "$final"
(cd "$saida" && sha256sum "pistick-$versao-rpi-arm64.img.xz" > "pistick-$versao-rpi-arm64.img.xz.sha256")
if [ -n "$dono" ]; then
    chown -R "$dono":"$dono" "$saida" 2>/dev/null || true
fi
sucesso=1
fim=$(date +%s)
echo
echo "imagem:    $final ($(wc -c < "$final") bytes)"
echo "sha256:    $(cut -d' ' -f1 "$final.sha256")"
echo "base:      $BASE_VERSAO ($tamanho_base bytes descompactada; $(wc -c < "$img") com o crescimento)"
echo "tempo:     $((fim - inicio)) s"
