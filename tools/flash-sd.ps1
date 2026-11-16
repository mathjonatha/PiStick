param(
    [int]$DiskNumber = 3,
    [string]$Root = "C:\Users\math\Documents\Raspberry",
    [string]$Python = "C:\Users\math\AppData\Local\Programs\Python\Python312\python.exe"
)
$ErrorActionPreference = 'Stop'
$cache = Join-Path $Root ".cache"
$log = Join-Path $cache "flash.log"
$image = Join-Path $cache "raspios-trixie-arm64-lite.img.xz"
"" | Out-File $log -Encoding utf8
function Log($m) { "$(Get-Date -Format HH:mm:ss) $m" | Tee-Object -FilePath $log -Append }

try {
    $d = Get-Disk -Number $DiskNumber
    Log "disco $DiskNumber = '$($d.FriendlyName)' bus=$($d.BusType) tamanho=$([math]::Round($d.Size/1GB,1))GB"
    if ($d.BusType -ne 'USB' -or $d.IsBoot -or $d.IsSystem -or $d.Size -gt 200GB -or $d.Size -lt 8GB -or $d.FriendlyName -notmatch 'SD') {
        throw "disco $DiskNumber nao parece o cartao SD; abortado sem tocar em nada"
    }
    if (-not (Test-Path $image)) { throw "imagem nao encontrada: $image" }

    $ssid = (netsh wlan show interfaces | Select-String '^\s*SSID\s*:\s*(.+)$' | Select-Object -First 1).Matches.Groups[1].Value.Trim()
    if ($ssid) {
        $tmp = Join-Path $cache "wifi"
        New-Item -ItemType Directory -Force $tmp | Out-Null
        netsh wlan export profile name="$ssid" key=clear folder="$tmp" | Out-Null
        Log "perfil Wi-Fi exportado para o SSID '$ssid'"
    } else { Log "sem Wi-Fi conectado; o Pi ficara so com cabo de rede" }

    Log "apagando a tabela de particoes do cartao"
    Clear-Disk -Number $DiskNumber -RemoveData -RemoveOEM -Confirm:$false
    Start-Sleep -Seconds 2

    Log "gravando a imagem (leva alguns minutos)"
    & $Python -I (Join-Path $Root "tools\flash_image.py") $image "\\.\PhysicalDrive$DiskNumber" 2>&1 | ForEach-Object { Log $_ }
    if ($LASTEXITCODE -ne 0) { throw "flash_image.py saiu com codigo $LASTEXITCODE" }

    Log "relendo particoes"
    Update-HostStorageCache
    "rescan" | diskpart | Out-Null
    $boot = $null
    foreach ($i in 1..20) {
        Start-Sleep -Seconds 2
        $boot = Get-Partition -DiskNumber $DiskNumber -ErrorAction SilentlyContinue | Where-Object { $_.Size -lt 2GB } | Select-Object -First 1
        if ($boot) { break }
    }
    if ($boot -and -not $boot.DriveLetter) {
        try { $boot | Add-PartitionAccessPath -AssignDriveLetter; $boot = Get-Partition -DiskNumber $DiskNumber -PartitionNumber $boot.PartitionNumber } catch { Log "sem letra automatica: $_" }
    }
    if ($boot -and $boot.DriveLetter) {
        Log "particao de boot montada em $($boot.DriveLetter):"
        & (Join-Path $Root "tools\write-bootconfig.ps1") -Root $Root 2>&1 | ForEach-Object { Log $_ }
    } else {
        Log "PENDENTE: particao de boot nao montou; reinsira o cartao e rode tools\write-bootconfig.ps1"
    }
    Log "FIM OK"
} catch {
    Log "FALHA: $_"
    exit 1
}
