#!/bin/sh
# Roda NO PI: prova do disco cifrado do marco 9B (docs/25, decisão 6). NÃO é o produto: é só a prova de que o kernel do Pi tem
# o Adiantum e de quanto ele custa, num ARQUIVO DE LAÇO (nunca em /dev/mmcblk* nem em partição do cartão).
#
#   sh tools/pi/provar-disco-cifrado.sh              prova com o arquivo na casa do usuário e, de novo, em /dev/shm (RAM)
#   sh tools/pi/provar-disco-cifrado.sh --so-casa    só com o arquivo na casa do usuário (o cartão SD)
#
# Precisa do pacote cryptsetup-bin (`sudo apt-get install -y --no-install-recommends cryptsetup-bin`; sem o
# cryptsetup-initramfs: o initramfs não pode ser regenerado por esta prova) e de sudo (laço, dm-crypt e montagem).
# Para cada local do arquivo de 256 MiB mede, com e sem cifra (a mesma base de laço, o mesmo ext4):
#   - bruto: dd de 256 MiB no dispositivo (escrita com oflag=direct conv=fsync, leitura com iflag=direct);
#   - sistema de arquivos: dd de 160 MiB num arquivo do ext4 (escrita com conv=fsync, leitura com iflag=direct).
# Cifras: xchacha20,aes-adiantum-plain64 (a do projeto) e aes-xts-plain64 (para comparar: o Pi 4 não tem AES em hardware).
# A chave é aleatória, vive numa pasta temporária 0700 e é apagada ao final. Tudo é desfeito, mesmo se algo falhar
# (trap): nenhum laço, mapeamento, montagem ou arquivo fica para trás.
set -eu

so_casa=0
for arg in "$@"; do
    case "$arg" in
        --so-casa) so_casa=1 ;;
        *)
            echo "uso: provar-disco-cifrado.sh [--so-casa]" >&2
            exit 2
            ;;
    esac
done
if [ "$(id -u)" -eq 0 ]; then
    echo "FALHOU: não rode como root; o roteiro chama sudo onde precisa." >&2
    exit 1
fi
PATH=$PATH:/usr/sbin:/sbin
for c in cryptsetup losetup dmsetup mkfs.ext4 dd; do
    command -v "$c" >/dev/null 2>&1 || {
        echo "FALHOU: falta o comando $c (sudo apt-get install -y --no-install-recommends cryptsetup-bin)." >&2
        exit 1
    }
done

trabalho=$(mktemp -d "$HOME/disco-prova.XXXXXX")
chmod 700 "$trabalho"
mapa=pistick-prova-adiantum
mapa_aes=pistick-prova-aes
laco=""
montado=""
abertos=""

limpar() {
    set +e
    if [ -n "$montado" ]; then sudo umount "$montado" 2>/dev/null; fi
    for m in $abertos; do sudo cryptsetup close "$m" 2>/dev/null; done
    if [ -n "$laco" ]; then sudo losetup -d "$laco" 2>/dev/null; fi
    rm -rf "$trabalho" "${shm:-/nonexistent}"
}
trap limpar EXIT INT TERM

# Só dispositivos de laço: nunca disco de verdade.
so_laco() {
    case "$1" in
        /dev/loop[0-9]*) ;;
        *)
            echo "FALHOU: recusado, '$1' não é um dispositivo de laço." >&2
            exit 1
            ;;
    esac
}

echo "== Kernel =="
uname -r
modulos=0
for m in adiantum nhpoly1305 chacha; do
    for f in "/lib/modules/$(uname -r)/kernel/crypto/$m".ko*; do
        if [ -e "$f" ]; then modulos=$((modulos + 1)); fi
    done
done
echo "módulos de cifra do kernel (adiantum, nhpoly1305, chacha): $modulos de 3 em /lib/modules/$(uname -r)/kernel/crypto"
config="/boot/config-$(uname -r)"
if [ -r "$config" ]; then grep -E '^CONFIG_(CRYPTO_ADIANTUM|CRYPTO_NHPOLY1305(_NEON)?|DM_CRYPT|BLK_DEV_LOOP)=' "$config"; fi

# Alvo das escritas: só laço ou mapeamento desta prova (nunca disco de verdade).
alvo_seguro() {
    case "$1" in
        /dev/loop[0-9]* | /dev/mapper/pistick-prova-*) ;;
        *)
            echo "FALHOU: recusado, '$1' não é laço nem mapeamento desta prova." >&2
            exit 1
            ;;
    esac
}

# chave: 64 bytes aleatórios (o Adiantum lê 32; o XTS de 512 bits lê 64). Nunca é impressa.
head -c 64 /dev/urandom > "$trabalho/chave"
chmod 600 "$trabalho/chave"

# vazao_dd: roda dd e imprime só "NNN MB/s" (o final da última linha).
vazao_dd() {
    "$@" 2>&1 | tail -n 1 | sed 's/.*, \([0-9.,]* [MGk]B\/s\)$/\1/'
}

# medir ROTULO DISPOSITIVO PONTO_DE_MONTAGEM_DO_EXT4_OU_VAZIO
medir_bruto() {
    alvo_seguro "$2"
    for i in 1 2 3; do
        e=$(vazao_dd sudo dd if=/dev/zero of="$2" bs=1M count=256 oflag=direct conv=fsync)
        l=$(vazao_dd sudo dd if="$2" of=/dev/null bs=1M count=256 iflag=direct)
        echo "  $1, bruto, rodada $i: escrita $e, leitura $l"
    done
}

medir_fs() {
    alvo_seguro "$2"
    sudo mkfs.ext4 -q -F "$2" >/dev/null
    montado="$trabalho/mnt"
    mkdir -p "$montado"
    sudo mount "$2" "$montado"
    for i in 1 2 3; do
        e=$(vazao_dd sudo dd if=/dev/zero of="$montado/dados" bs=1M count=160 conv=fsync)
        l=$(vazao_dd sudo dd if="$montado/dados" of=/dev/null bs=1M iflag=direct)
        echo "  $1, ext4 (160 MiB), rodada $i: escrita $e, leitura $l"
        sudo rm -f "$montado/dados"
    done
    sudo umount "$montado"
    montado=""
}

# prova_local ROTULO ARQUIVO
prova_local() {
    rotulo=$1
    arq=$2
    echo "== $rotulo: $arq (256 MiB) =="
    rm -f "$arq"
    truncate -s 256M "$arq"
    laco=$(sudo losetup --find --show "$arq")
    so_laco "$laco"
    echo "laço: $laco"

    echo "-- sem cifra"
    medir_bruto "sem cifra" "$laco"
    medir_fs "sem cifra" "$laco"

    echo "-- xchacha20,aes-adiantum-plain64"
    sudo cryptsetup open --type plain --cipher xchacha20,aes-adiantum-plain64 --key-size 256 --key-file "$trabalho/chave" --batch-mode "$laco" "$mapa"
    abertos="$mapa"
    sudo cryptsetup status "$mapa" | grep -E 'type|cipher|keysize|device:' | sed 's/^ */  /'
    # O Adiantum tem de aparecer no kernel (carregado sob demanda pelo dm-crypt).
    echo "  /proc/crypto: $(grep -c -i adiantum /proc/crypto) entradas com adiantum"
    # Cifrou de verdade? Um marcador escrito pelo mapeamento não pode aparecer no dispositivo de laço.
    echo "MARCADOR-PISTICK-CLARO-MARCADOR-PISTICK-CLARO" | sudo dd of="/dev/mapper/$mapa" bs=512 count=1 conv=fsync,sync 2>/dev/null
    lido=$(sudo dd if="/dev/mapper/$mapa" bs=512 count=1 2>/dev/null | grep -c -a 'MARCADOR-PISTICK-CLARO' || true)
    bruto=$(sudo dd if="$laco" bs=512 count=1 2>/dev/null | grep -c -a 'MARCADOR-PISTICK-CLARO' || true)
    echo "  marcador lido pelo mapeamento: $lido; visível no dispositivo de laço (deve ser 0): $bruto"
    sudo cryptsetup close "$mapa"
    abertos=""
    sudo cryptsetup open --type plain --cipher xchacha20,aes-adiantum-plain64 --key-size 256 --key-file "$trabalho/chave" --batch-mode "$laco" "$mapa"
    abertos="$mapa"
    medir_bruto "adiantum" "/dev/mapper/$mapa"
    medir_fs "adiantum" "/dev/mapper/$mapa"
    sudo cryptsetup close "$mapa"
    abertos=""

    echo "-- aes-xts-plain64 (comparação; sem AES em hardware no Pi 4)"
    sudo cryptsetup open --type plain --cipher aes-xts-plain64 --key-size 512 --key-file "$trabalho/chave" --batch-mode "$laco" "$mapa_aes"
    abertos="$mapa_aes"
    medir_bruto "aes-xts" "/dev/mapper/$mapa_aes"
    medir_fs "aes-xts" "/dev/mapper/$mapa_aes"
    sudo cryptsetup close "$mapa_aes"
    abertos=""

    sudo losetup -d "$laco"
    laco=""
    rm -f "$arq"
}

prova_local "casa do usuário (cartão SD)" "$trabalho/disco.img"
shm=""
if [ "$so_casa" -eq 0 ] && [ -d /dev/shm ]; then
    shm=$(mktemp -d /dev/shm/pistick-prova.XXXXXX)
    prova_local "RAM (/dev/shm): mede só o custo da cifra, sem o cartão" "$shm/disco.img"
fi

echo "== Ao final =="
limpar
trap - EXIT INT TERM
echo "laços ativos:"
losetup -a
echo "mapeamentos (dmsetup ls):"
sudo dmsetup ls
sobras=0
for d in "$HOME"/disco-prova.*; do
    if [ -e "$d" ]; then sobras=$((sobras + 1)); fi
done
echo "pastas disco-prova sobrando em ~: $sobras"
