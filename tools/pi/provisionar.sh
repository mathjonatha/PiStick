#!/bin/sh
# Roda NO PI (Raspberry Pi OS, aarch64): leva o sistema ao estado do projeto PiStick e registra o
# serviço pistick (psd no boot). Idempotente: cada passo confere antes de agir e imprime
# ok, feito, FALTA (só em --verificar) ou FALHOU. Veja docs/15-provisionamento.md.
#
#   sh tools/pi/provisionar.sh               instala tudo e habilita o serviço
#   sh tools/pi/provisionar.sh --verificar   não muda nada; código 1 se algo falta
#   sh tools/pi/provisionar.sh --dev         além do básico, as ferramentas de desenvolvimento
#   sh tools/pi/provisionar.sh --sem-servico instala, mas não habilita nem inicia o serviço
#   sh tools/pi/provisionar.sh --usuario NOME  o serviço roda como NOME (criado como usuário de sistema, sem senha e sem
#                                              shell de login, se não existir; grupos video e render). Padrão: quem roda isto.
set -eu

verificar=0
dev=0
servico=1
usuario_arg=""
uso="uso: provisionar.sh [--verificar] [--dev] [--sem-servico] [--usuario NOME]"
while [ $# -gt 0 ]; do
    case "$1" in
        --verificar) verificar=1 ;;
        --dev) dev=1 ;;
        --sem-servico) servico=0 ;;
        --usuario)
            if [ $# -lt 2 ]; then
                echo "$uso" >&2
                exit 2
            fi
            usuario_arg=$2
            shift
            ;;
        *)
            echo "$uso" >&2
            exit 2
            ;;
    esac
    shift
done
# Nome de usuário válido (minúsculas, dígitos, _ e -; não começa por dígito nem por hífen; até 32 caracteres).
if [ -n "$usuario_arg" ]; then
    case "$usuario_arg" in
        *[!a-z0-9_-]* | -* | [0-9]*)
            echo "FALHOU: --usuario: nome inválido." >&2
            exit 2
            ;;
    esac
    if [ "${#usuario_arg}" -gt 32 ]; then
        echo "FALHOU: --usuario: nome com mais de 32 caracteres." >&2
        exit 2
    fi
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
invocador=$(id -un)
usuario=${usuario_arg:-$invocador}
if [ "$usuario" = "$invocador" ]; then
    casa=$HOME
elif getent passwd "$usuario" >/dev/null 2>&1; then
    casa=$(getent passwd "$usuario" | cut -d: -f6)
else
    casa=/home/$usuario
fi
falhas=0
resumo=""
cargo_bin="$HOME/.cargo/bin"
unidade=/etc/systemd/system/pistick.service
conf=/etc/pistick/psd.conf
destino_psd=/usr/local/bin/psd
destino_servico=/usr/local/libexec/pistick/iniciar-servico.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

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

# falta NOME [DETALHE]: só em --verificar; marca o que falta.
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
    if [ -f "$3" ] && cmp -s "$2" "$3"; then
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

# 1 e 2. Pacotes.
# A lista de execução mora em tools/pi/pacotes-execucao.txt (única; vai também para o pacote).
lista_execucao=$(grep -v '^[[:space:]]*#' "$repo/tools/pi/pacotes-execucao.txt" | grep -v '^[[:space:]]*$' | tr '\n' ' ')
# shellcheck disable=SC2086
pacotes "execução" $lista_execucao
pacotes "compilação" build-essential pkg-config libxkbcommon-dev curl ca-certificates

# 3. Rust (não atualiza um que já existe).
if [ -x "$cargo_bin/cargo" ]; then
    estado "rust (rustup)" ok "$("$cargo_bin/cargo" --version)"
elif [ "$verificar" -eq 1 ]; then
    falta "rust (rustup)" "falta $cargo_bin/cargo"
else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$tmp/rustup-init.sh" || falhou "rust (rustup)" "download"
    sh "$tmp/rustup-init.sh" -y --profile minimal --default-toolchain stable -c clippy || falhou "rust (rustup)" "rustup-init"
    estado "rust (rustup)" feito
fi

# 4. Compilação e instalação do psd.
binario="$repo/target/release/psd"
psd_mudou=0
if [ "$verificar" -eq 1 ]; then
    if [ -f "$destino_psd" ] && [ -f "$binario" ] && cmp -s "$binario" "$destino_psd"; then
        estado "psd instalado" ok "$destino_psd, igual ao target/release"
    elif [ -f "$destino_psd" ]; then
        estado "psd instalado" ok "$destino_psd, difere do target/release (rode sem --verificar para atualizar)"
    else
        falta "psd instalado" "falta $destino_psd"
    fi
else
    PATH="$cargo_bin:$PATH"
    (cd "$repo" && cargo build --release -p ps-server) || falhou "compilação do psd" "cargo build"
    estado "compilação do psd" ok
    instalar_arquivo "psd instalado" "$binario" "$destino_psd" 755
    psd_mudou=$mudou
fi

# 4b. Usuário do serviço (só com --usuario e diferente de quem roda o roteiro): usuário de sistema, sem senha e sem shell de
# login. Nunca recebe sudo. O systemd passa a posse de /var/lib/pistick a ele quando o serviço sobe (StateDirectory).
if [ "$usuario" != "$invocador" ]; then
    if id -u "$usuario" >/dev/null 2>&1; then
        if [ "$(id -u "$usuario")" -eq 0 ]; then
            falhou "usuário do serviço" "$usuario é o root"
        fi
        if id -nG "$usuario" | tr ' ' '
' | grep -qx sudo; then
            estado "usuário do serviço" ok "$usuario (AVISO: está no grupo sudo)"
        else
            estado "usuário do serviço" ok "$usuario"
        fi
    elif [ "$verificar" -eq 1 ]; then
        falta "usuário do serviço" "$usuario não existe"
    else
        sudo useradd --system --user-group --create-home --home-dir "/home/$usuario" --shell /usr/sbin/nologin "$usuario" || falhou "usuário do serviço" "useradd"
        sudo chmod 700 "/home/$usuario" || falhou "usuário do serviço" "chmod da casa"
        estado "usuário do serviço" feito "$usuario criado (sistema, sem senha, sem shell de login)"
    fi
fi

# 5. Grupos: video (codificador em /dev/video11) e render (GPU em /dev/dri/renderD128).
for grupo in video render; do
    if ! getent group "$grupo" >/dev/null 2>&1; then
        estado "grupo $grupo" ok "o grupo não existe neste sistema"
    elif ! id -u "$usuario" >/dev/null 2>&1; then
        estado "grupo $grupo" ok "pendente: $usuario ainda não existe (--verificar)"
    elif id -nG "$usuario" | tr ' ' '
' | grep -qx "$grupo"; then
        estado "grupo $grupo" ok "$usuario"
    elif [ "$verificar" -eq 1 ]; then
        falta "grupo $grupo" "$usuario fora do grupo"
    else
        sudo usermod -aG "$grupo" "$usuario" || falhou "grupo $grupo" "usermod"
        estado "grupo $grupo" feito "$usuario; vale no próximo login; o serviço já recebe o grupo"
    fi
done

# 6. Configuração (nunca sobrescreve).
if [ -f "$conf" ]; then
    estado "configuração" ok "$conf"
elif [ "$verificar" -eq 1 ]; then
    falta "configuração" "falta $conf"
else
    cat > "$tmp/psd.conf" <<'CONF'
# Opções do psd no serviço. Um app por linha em APP_n (n de 1 a 8); linhas vazias são ignoradas.
LISTEN=0.0.0.0:7700
PSD_OPTS=
APP_1=foot
APP_2=
APP_3=
CONF
    sudo install -D -m 644 "$tmp/psd.conf" "$conf" || falhou "configuração" "$conf"
    estado "configuração" feito "$conf"
fi

# 7. Serviço.
instalar_arquivo "script do serviço" "$repo/tools/pi/iniciar-servico.sh" "$destino_servico" 755
script_mudou=$mudou

# Atalho do marco 9A: `pistick-pair` roda `psd pair` como o usuário do serviço, no soquete do serviço.
sed -e "s|@USUARIO@|$usuario|g" "$repo/tools/pi/pistick-pair.in" > "$tmp/pistick-pair"
instalar_arquivo "atalho pistick-pair" "$tmp/pistick-pair" /usr/local/bin/pistick-pair 755

# Wi-Fi sem economia de energia (decisão do usuário, docs/24 e docs/15): o NetworkManager lê o arquivo na próxima
# ativação da conexão. Este roteiro NÃO reinicia o NetworkManager nem mexe em conexão. Só se o NetworkManager existe.
nm_conf=/etc/NetworkManager/conf.d/pistick-wifi-sem-economia.conf
if [ -d /etc/NetworkManager ] || command -v NetworkManager >/dev/null 2>&1; then
    instalar_arquivo "Wi-Fi sem economia de energia" "$repo/tools/pi/networkmanager/pistick-wifi-sem-economia.conf" "$nm_conf" 644
else
    estado "Wi-Fi sem economia de energia" ok "NetworkManager não existe neste sistema; ignorado"
fi

# Configuração do PipeWire do projeto (marco 7): o psd a lê em /etc/pistick/pipewire.
pw_mudou=0
# Todos os .conf da pasta (marco 7B: a mesma pipewire.conf traz a fonte virtual do microfone, `pistick-mic`).
for arq in "$repo"/tools/pi/pipewire/*.conf; do
    f=$(basename "$arq")
    instalar_arquivo "configuração do PipeWire: $f" "$arq" "/etc/pistick/pipewire/$f" 644
    if [ "$mudou" -eq 1 ]; then pw_mudou=1; fi
done
if grep -q 'node.name = pistick-mic[[:space:]]*$' /etc/pistick/pipewire/pipewire.conf 2>/dev/null; then
    estado "fonte do microfone (pistick-mic)" ok "na configuração instalada"
else
    falta "fonte do microfone (pistick-mic)" "a configuração instalada em /etc/pistick/pipewire não a tem"
fi

# Shell dos apps do serviço: o do usuário, ou /bin/bash se for nologin (o foot e outros terminais usam $SHELL).
shell_servico=$(getent passwd "$usuario" 2>/dev/null | cut -d: -f7 || true)
case "${shell_servico:-}" in
    "" | */nologin | */false) shell_servico=/bin/bash ;;
    *) ;;
esac
sed -e "s|@USUARIO@|$usuario|g" -e "s|@CASA@|$casa|g" -e "s|@SHELL@|$shell_servico|g" "$repo/tools/pi/pistick.service.in" > "$tmp/pistick.service"
instalar_arquivo "unidade pistick.service" "$tmp/pistick.service" "$unidade" 644
unidade_mudou=$mudou
if [ "$verificar" -eq 0 ] && [ "$unidade_mudou" -eq 1 ]; then
    sudo systemctl daemon-reload || falhou "systemd" "daemon-reload"
fi

if [ "$servico" -eq 1 ]; then
    ativo=0
    habilitado=0
    if systemctl is-active --quiet pistick 2>/dev/null; then ativo=1; fi
    if systemctl is-enabled --quiet pistick 2>/dev/null; then habilitado=1; fi
    if [ "$verificar" -eq 1 ]; then
        if [ "$habilitado" -eq 1 ]; then estado "serviço habilitado" ok; else falta "serviço habilitado"; fi
        if [ "$ativo" -eq 1 ]; then estado "serviço ativo" ok; else falta "serviço ativo"; fi
    elif [ "$habilitado" -eq 1 ] && [ "$ativo" -eq 1 ]; then
        # Reinicia só se algo que o serviço usa mudou nesta execução.
        if [ "$psd_mudou" -eq 1 ] || [ "$script_mudou" -eq 1 ] || [ "$unidade_mudou" -eq 1 ] || [ "$pw_mudou" -eq 1 ]; then
            sudo systemctl restart pistick || falhou "serviço" "restart"
            estado "serviço" feito "reiniciado: binário, script, unidade ou configuração do PipeWire mudou"
        else
            estado "serviço" ok "habilitado e ativo"
        fi
    else
        sudo systemctl enable --now pistick || falhou "serviço" "enable --now"
        estado "serviço" feito "habilitado e iniciado"
    fi
else
    estado "serviço" ok "ignorado (--sem-servico)"
fi

# 8. Ferramentas de desenvolvimento.
if [ "$dev" -eq 1 ]; then
    pacotes "desenvolvimento" v4l-utils ffmpeg avahi-utils shellcheck strace htop parted xz-utils zerofree systemd-container
fi

# 9. Resumo.
echo
echo "== Resumo =="
echo "$resumo" | sed '1{/^$/d}'
echo
echo "Log do serviço: journalctl -u pistick -e"
if [ "$falhas" -ne 0 ]; then
    echo "Algo falta (veja as linhas FALTA acima)."
    exit 1
fi
