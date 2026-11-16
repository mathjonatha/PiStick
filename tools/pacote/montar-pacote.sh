#!/bin/sh
set -eu

saida="$HOME/pistick-saida"
while [ $# -gt 0 ]; do
    case "$1" in
        --saida)
            [ $# -ge 2 ] || { echo "uso: montar-pacote.sh [--saida PASTA]" >&2; exit 2; }
            saida=$2
            shift 2
            ;;
        *)
            echo "uso: montar-pacote.sh [--saida PASTA]" >&2
            exit 2
            ;;
    esac
done

if [ "$(id -u)" -eq 0 ]; then
    echo "FALHOU: não rode como root." >&2
    exit 1
fi
if [ "$(uname -m)" != "aarch64" ]; then
    echo "FALHOU: só roda em aarch64." >&2
    exit 1
fi
case "$saida" in
    /dev/*)
        echo "FALHOU: a pasta de saída não pode ser um caminho de /dev." >&2
        exit 1
        ;;
esac

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"

base=$(tr -d '\r' < Cargo.toml | awk '/^\[workspace\.package\]/ { dentro = 1; next } /^\[/ { dentro = 0 } dentro && /^version[[:space:]]*=/ { split($0, partes, "\""); print partes[2]; exit }')
if [ -z "$base" ]; then
    echo "FALHOU: não achei a versão em [workspace.package] de Cargo.toml." >&2
    exit 1
fi
if [ -f VERSAO_GIT ]; then
    gitv=$(head -n 1 VERSAO_GIT | tr -d '[:space:]')
else
    gitv=""
fi
if [ -n "$gitv" ]; then
    versao="$base-g$gitv"
else
    versao="$base-sem-git"
fi
nome="pistick-$versao-aarch64.tar.xz"

for f in tools/pi/iniciar-servico.sh tools/pi/primeiro-boot.sh tools/pi/pistick.service.in \
    tools/pi/pistick-primeiro-boot.service tools/pi/pacotes-execucao.txt tools/pacote/psd.conf tools/instalar.sh \
    tools/pi/networkmanager/pistick-wifi-sem-economia.conf; do
    if [ ! -f "$f" ]; then
        echo "FALHOU: falta $f no repositório." >&2
        exit 1
    fi
done

PATH="$HOME/.cargo/bin:$PATH"
echo "== Compilando o psd ($versao) =="
if [ -f Cargo.lock ]; then
    cargo build --release --locked -p ps-server
else
    cargo build --release -p ps-server
fi
binario="$repo/target/release/psd"
[ -x "$binario" ] || { echo "FALHOU: $binario não existe." >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
raiz="$tmp/pistick-$versao"
mkdir -p "$raiz/bin" "$raiz/libexec" "$raiz/systemd" "$raiz/etc/NetworkManager"

strip -o "$raiz/bin/psd" "$binario"
chmod 755 "$raiz/bin/psd"
install -m 755 tools/pi/iniciar-servico.sh "$raiz/libexec/iniciar-servico.sh"
install -m 755 tools/pi/primeiro-boot.sh "$raiz/libexec/primeiro-boot.sh"
install -m 644 tools/pi/pistick.service.in "$raiz/systemd/pistick.service.in"
install -m 644 tools/pi/pistick-primeiro-boot.service "$raiz/systemd/pistick-primeiro-boot.service"
install -m 644 tools/pacote/psd.conf "$raiz/etc/psd.conf"
install -m 644 tools/pi/networkmanager/pistick-wifi-sem-economia.conf "$raiz/etc/NetworkManager/pistick-wifi-sem-economia.conf"
install -m 755 tools/instalar.sh "$raiz/instalar-local.sh"
printf '%s\n' "$versao" > "$raiz/VERSAO"
chmod 644 "$raiz/VERSAO"
grep -v '^[[:space:]]*#' tools/pi/pacotes-execucao.txt | grep -v '^[[:space:]]*$' > "$raiz/PACOTES"
chmod 644 "$raiz/PACOTES"
find "$tmp" -type d -exec chmod 755 {} +

mkdir -p "$saida"
echo "== Empacotando =="
tar --sort=name --owner=0 --group=0 --numeric-owner --format=gnu \
    --mtime='2026-01-01 00:00:00 UTC' -cf - -C "$tmp" "pistick-$versao" \
    | xz -T1 -6 -c > "$tmp/$nome"
mv -f "$tmp/$nome" "$saida/$nome"
(cd "$saida" && sha256sum "$nome" > "$nome.sha256")

echo
echo "versão:   $versao"
echo "pacote:   $saida/$nome ($(wc -c < "$saida/$nome") bytes)"
echo "sha256:   $(cut -d' ' -f1 "$saida/$nome.sha256")"
