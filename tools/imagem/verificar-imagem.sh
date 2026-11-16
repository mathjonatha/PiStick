#!/bin/sh
set -eu

if [ $# -ne 1 ]; then
    echo "uso: sudo sh verificar-imagem.sh ARQUIVO.img[.xz]" >&2
    exit 2
fi
arq=$1

falhou() {
    echo "FALHOU: $*" >&2
    exit 1
}

case "$arq" in
    /dev | /dev/* | /proc | /proc/* | /sys | /sys/*) falhou "'$arq' é um caminho de dispositivo; só aceito arquivos de imagem." ;;
esac
real=$(readlink -f "$arq" 2>/dev/null || printf '%s' "$arq")
if [ -b "$arq" ] || [ -b "$real" ]; then
    falhou "'$arq' é um dispositivo de bloco; só aceito arquivos de imagem."
fi
case "$real" in
    /dev | /dev/* | /proc | /proc/* | /sys | /sys/*) falhou "'$arq' resolve para '$real', um caminho de dispositivo." ;;
esac
[ -f "$arq" ] || falhou "o arquivo '$arq' não existe."
[ "$(id -u)" -eq 0 ] || falhou "rode com sudo."
for c in losetup mount umount xz mountpoint find; do
    command -v "$c" >/dev/null 2>&1 || falhou "falta o comando '$c'."
done

tmp=$(mktemp -d /var/tmp/pistick-verif.XXXXXX)
mnt="$tmp/m"
mkdir "$mnt"
laco=""
img=$real

limpar() {
    set +e
    trap '' INT TERM HUP
    for m in "$mnt/boot/firmware" "$mnt"; do
        n=0
        while mountpoint -q "$m" 2>/dev/null; do
            umount "$m" 2>/dev/null && break
            n=$((n + 1))
            if [ "$n" -ge 5 ]; then
                umount -l "$m" 2>/dev/null
                break
            fi
            sleep 1
        done
    done
    if [ -n "$laco" ]; then losetup -d "$laco" 2>/dev/null; fi
    rm -rf "$tmp"
}
trap limpar EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

case "$arq" in
    *.xz)
        echo "descompactando $arq (temporário)..."
        img="$tmp/imagem.img"
        xz -dc "$arq" > "$img" || falhou "não consegui descompactar."
        ;;
esac

laco=$(losetup --read-only --partscan --find --show "$img") || falhou "losetup"
case "$laco" in
    /dev/loop[0-9] | /dev/loop[0-9][0-9] | /dev/loop[0-9][0-9][0-9]) ;;
    *) falhou "alvo '$laco' não é um dispositivo de laço." ;;
esac
n=0
while [ ! -b "${laco}p2" ]; do
    n=$((n + 1))
    [ "$n" -le 50 ] || falhou "o nó ${laco}p2 não apareceu."
    sleep 0.2
done
mount -o ro,noload "${laco}p2" "$mnt" || falhou "mount somente leitura da raiz"
if [ -b "${laco}p1" ]; then
    mount -o ro "${laco}p1" "$mnt/boot/firmware" 2>/dev/null || true
fi

falhas=0
total=0
ok() {
    total=$((total + 1))
    printf 'ok      %s\n' "$1"
}
no() {
    total=$((total + 1))
    falhas=$((falhas + 1))
    printf 'FALHOU  %s\n' "$1"
}
checa() {
    desc=$1
    shift
    if "$@" >/dev/null 2>&1; then ok "$desc"; else no "$desc"; fi
}
ausentes() {
    for a in "$@"; do
        if [ -e "$a" ] || [ -L "$a" ]; then return 1; fi
    done
    return 0
}

elf() {
    [ -x "$1" ] && [ "$(dd if="$1" bs=1 skip=1 count=3 2>/dev/null)" = ELF ]
}
hostname_pistick() {
    [ "$(tr -d '[:space:]' < "$1")" = pistick ]
}
senha_bloqueada() {
    campo=$(awk -F: '$1 == "pistick" { print $2 }' "$1")
    case "$campo" in
        '!'* | '*'*) return 0 ;;
        *) return 1 ;;
    esac
}
sem_sudo() {
    if grep -E '^sudo:[^:]*:[^:]*:(.*,)?pistick(,|$)' "$1"; then return 1; fi
    return 0
}
sem_achados() {
    [ -z "$(find "$@" -print -quit 2>/dev/null)" ]
}
machine_id_base() {
    c=$(tr -d '[:space:]' < "$1")
    [ -z "$c" ] || [ "$c" = uninitialized ]
}

m=$mnt
echo "imagem: $arq"
[ -f "$m/usr/local/share/pistick/VERSAO" ] && echo "versão: $(head -n 1 "$m/usr/local/share/pistick/VERSAO")"

checa "psd instalado (ELF)" elf "$m/usr/local/bin/psd"
checa "iniciar-servico.sh" test -x "$m/usr/local/libexec/pistick/iniciar-servico.sh"
checa "primeiro-boot.sh" test -x "$m/usr/local/libexec/pistick/primeiro-boot.sh"
checa "unidade pistick.service (User=pistick)" grep -q '^User=pistick$' "$m/etc/systemd/system/pistick.service"
checa "unidade pistick-primeiro-boot.service" test -f "$m/etc/systemd/system/pistick-primeiro-boot.service"
checa "psd.conf" test -f "$m/etc/pistick/psd.conf"
checa "pistick.service habilitado" test -L "$m/etc/systemd/system/multi-user.target.wants/pistick.service"
checa "pistick-primeiro-boot.service habilitado" test -L "$m/etc/systemd/system/multi-user.target.wants/pistick-primeiro-boot.service"
checa "nome do aparelho: pistick" hostname_pistick "$m/etc/hostname"

checa "usuário pistick existe, home /home/pistick" grep -q '^pistick:[^:]*:[0-9]*:[0-9]*:[^:]*:/home/pistick:' "$m/etc/passwd"
checa "senha do pistick bloqueada" senha_bloqueada "$m/etc/shadow"
checa "pistick fora do grupo sudo" sem_sudo "$m/etc/group"

checa "sem chaves SSH de máquina" ausentes "$m"/etc/ssh/ssh_host_*
if [ -z "$(find "$m/etc" "$m/home" "$m/root" "$m/var" "$m/usr/local" -name 'authorized_keys*' -print -quit 2>/dev/null)" ]; then
    ok "sem authorized_keys"
else
    no "sem authorized_keys"
fi
if [ -z "$(find "$m/etc/NetworkManager/system-connections" "$m/etc/netplan" -mindepth 1 -type f -print -quit 2>/dev/null)" ]; then
    ok "sem perfis de rede (NetworkManager, netplan)"
else
    no "sem perfis de rede (NetworkManager, netplan)"
fi
if [ -d "$m/boot/firmware" ]; then
    achados=""
    for f in user-data network-config; do
        if [ -f "$m/boot/firmware/$f" ] && grep -v '^[[:space:]]*#' "$m/boot/firmware/$f" | grep -qiE 'ssid|password|passwd|authorized|plain_text'; then
            achados="$achados $f"
        fi
    done
    if [ -z "$achados" ]; then ok "boot/firmware sem credenciais (user-data, network-config)"; else no "boot/firmware com credenciais em:$achados"; fi
fi

nomes=""
if command -v nmcli >/dev/null 2>&1; then
    for c in $(nmcli -t -f TYPE,NAME connection show 2>/dev/null | sed -n 's/^802-11-wireless://p' | tr ' ' '_'); do
        c=$(printf '%s' "$c" | tr '_' ' ')
        s=$(nmcli -g 802-11-wireless.ssid connection show "$c" 2>/dev/null || true)
        nomes="$nomes
$c
$s"
    done
fi
achou_rede=0
tentou_rede=0
OLDIFS=$IFS
IFS='
'
for nome in $nomes; do
    [ "${#nome}" -ge 3 ] || continue
    tentou_rede=1
    if grep -rIlsF -e "$nome" "$m/etc" "$m/home" "$m/root" "$m/var" "$m/usr/local" "$m/boot/firmware" 2>/dev/null | grep -q .; then
        achou_rede=1
    fi
done
IFS=$OLDIFS
if [ "$tentou_rede" -eq 0 ]; then
    ok "nome da rede do desenvolvimento: pulado (nenhuma rede Wi-Fi no Pi para comparar)"
elif [ "$achou_rede" -eq 0 ]; then
    ok "nenhuma ocorrência do nome da rede do desenvolvimento"
else
    no "há ocorrência do nome da rede do desenvolvimento (não impresso)"
fi

checa "sem ~/.cargo, rustup, rustc, cargo" ausentes \
    "$m/root/.cargo" "$m/root/.rustup" "$m/home/pistick/.cargo" "$m/home/pi/.cargo" "$m/usr/bin/rustc" "$m/usr/bin/cargo" \
    "$m/usr/local/bin/rustc" "$m/usr/local/bin/cargo" "$m/usr/local/bin/rustup"
if ausentes "$m/usr/bin/gcc" "$m/usr/bin/cc"; then
    ok "sem compilador C"
else
    ok "aviso: gcc veio da imagem base oficial (nada do PiStick o instala)"
fi
checa "sem repositório do projeto (Cargo.toml, .git)" sem_achados \
    "$m/home" "$m/root" "$m/opt" "$m/usr/local" "$m/srv" '(' -name Cargo.toml -o -name .git ')'

casa_ok=1
esqueleto=$(find "$m/etc/skel" -mindepth 1 -printf '%f\n' 2>/dev/null | sort)
for d in "$m"/home/*; do
    [ -d "$d" ] || continue
    nome=$(basename "$d")
    case "$nome" in
        pi | pistick) ;;
        *) casa_ok=0 ;;
    esac
    for e in $(find "$d" -mindepth 1 -printf '%f\n' 2>/dev/null | sort); do
        if ! printf '%s\n' "$esqueleto" | grep -qxF -e "$e"; then casa_ok=0; fi
    done
done
if [ "$casa_ok" -eq 1 ]; then ok "/home só com o esqueleto (pi da base e pistick)"; else no "/home tem algo além do esqueleto"; fi

checa "listas do apt vazias" sem_achados "$m/var/lib/apt/lists" -type f ! -name lock
checa "cache do apt vazio" sem_achados "$m/var/cache/apt/archives" -name '*.deb'
checa "/var/log sem arquivos" sem_achados "$m/var/log" -type f
checa "sem históricos de shell" sem_achados "$m/root" "$m/home" -name .bash_history
checa "machine-id como na base (uninitialized ou vazio)" machine_id_base "$m/etc/machine-id"

echo
echo "$total itens, $falhas falharam."
if [ "$falhas" -ne 0 ]; then exit 1; fi
