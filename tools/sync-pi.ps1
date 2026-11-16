$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$sujo = git status --porcelain --untracked-files=no
if ($sujo) { Write-Warning "Há alterações não commitadas em arquivos rastreados; só o HEAD é enviado." }

$hash = (git rev-parse --short HEAD).Trim()
$remoto = "mkdir -p pistick && find pistick -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + && tar -x -C pistick && echo $hash > pistick/VERSAO_GIT"
cmd /c "git archive HEAD | ssh pi-stick `"$remoto`""
if ($LASTEXITCODE -ne 0) { throw "Falha ao sincronizar (código $LASTEXITCODE)" }
Write-Host "Sincronizado: $(git rev-parse --short HEAD) -> pi-stick:~/pistick"
