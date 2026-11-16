# Envia para ~/pistick no Pi exatamente os arquivos rastreados pelo git (commit HEAD).
# Preserva ~/pistick/target. Pode ser chamado de novo a qualquer momento.
# O pipe passa por cmd.exe porque o PowerShell 5.1 corrompe bytes em pipes binários.
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$sujo = git status --porcelain --untracked-files=no
if ($sujo) { Write-Warning "Há alterações não commitadas em arquivos rastreados; só o HEAD é enviado." }

# Limpa tudo em ~/pistick, menos target, para refletir remoções; depois extrai.
# VERSAO_GIT: o git archive não leva o .git; o hash curto do commit vai num arquivo na raiz enviada
# (lido por tools/pacote/montar-pacote.sh; sem o arquivo a versão fica sem-git).
$hash = (git rev-parse --short HEAD).Trim()
$remoto = "mkdir -p pistick && find pistick -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + && tar -x -C pistick && echo $hash > pistick/VERSAO_GIT"
cmd /c "git archive HEAD | ssh pi-stick `"$remoto`""
if ($LASTEXITCODE -ne 0) { throw "Falha ao sincronizar (código $LASTEXITCODE)" }
Write-Host "Sincronizado: $(git rev-parse --short HEAD) -> pi-stick:~/pistick"
