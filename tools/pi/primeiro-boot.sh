#!/bin/sh
# Instalado em /usr/local/libexec/pistick/primeiro-boot.sh e chamado, uma só vez, pela unidade
# pistick-primeiro-boot.service (só existe na imagem pronta). Se o nome do aparelho ainda é "pistick",
# troca para pistick-xxxx (quatro últimos dígitos hexadecimais do número de série), para dois
# aparelhos na mesma rede não colidirem na descoberta. Um nome definido pelo usuário (Imager,
# cloud-init) é respeitado. Ao final a unidade se desabilita.
set -eu

atual=""
if [ -f /etc/hostname ]; then
    atual=$(head -n 1 /etc/hostname | tr -d '[:space:]')
fi

serie=""
if [ -r /proc/device-tree/serial-number ]; then
    serie=$(tr -d '\0' < /proc/device-tree/serial-number | tr -d '[:space:]')
fi
if [ -z "$serie" ] && [ -r /proc/cpuinfo ]; then
    serie=$(sed -n 's/^Serial[[:space:]]*:[[:space:]]*//p' /proc/cpuinfo | head -n 1 | tr -d '[:space:]')
fi

desabilitar() {
    systemctl disable pistick-primeiro-boot.service >/dev/null 2>&1 || true
}

if [ "$atual" != "pistick" ]; then
    echo "primeiro-boot: o nome é '$atual', não 'pistick'; mantido."
    desabilitar
    exit 0
fi

# Só dígitos hexadecimais; os quatro últimos, em minúsculas.
quatro=$(printf '%s' "$serie" | tr 'A-F' 'a-f' | tr -cd '0-9a-f' | tail -c 4)
if [ "${#quatro}" -ne 4 ]; then
    echo "primeiro-boot: AVISO: número de série ilegível; o nome fica 'pistick'."
    desabilitar
    exit 0
fi

novo="pistick-$quatro"
printf '%s\n' "$novo" > /etc/hostname.novo
mv -f /etc/hostname.novo /etc/hostname
hostname "$novo" 2>/dev/null || true

if grep -q '^127\.0\.1\.1[[:space:]]' /etc/hosts 2>/dev/null; then
    sed -i "s/^\(127\.0\.1\.1[[:space:]]\{1,\}\).*/\1$novo/" /etc/hosts
else
    printf '127.0.1.1\t%s\n' "$novo" >> /etc/hosts
fi

echo "primeiro-boot: nome do aparelho: $novo"
desabilitar
