param(
    [string]$Address = '192.168.0.11:7700',
    [string[]]$Modos = @('auto', 'off'),
    [int[]]$MaxQp = @(0),
    [string]$Clip = 'mandelbrot-720p30.mp4',
    [int]$Segundos = 20,
    [string]$Psc = "$PSScriptRoot\..\target\release\psc.exe",
    [string]$Roteiro = "$PSScriptRoot\roteiros\63-medicao-mpv.txt",
    [string]$OutDir = "$PSScriptRoot\..\.cache\medicao4"
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$inv = [Globalization.CultureInfo]::InvariantCulture
function Med($v) { $s = @($v | Sort-Object); if ($s.Count -eq 0) { return 0 }; $s[[int][math]::Floor(($s.Count - 1) / 2)] }
function Ssh($cmd, $seg = 40) {
    $p = Start-Process ssh -ArgumentList '-n', 'pi-stick', $cmd -PassThru -NoNewWindow -RedirectStandardOutput "$OutDir\ssh.out" -RedirectStandardError "$OutDir\ssh.err"
    if (-not $p.WaitForExit($seg * 1000)) { Stop-Process -Id $p.Id -Force; Write-Warning "ssh passou de $seg s: $cmd" }
}

foreach ($qp in $MaxQp) {
foreach ($modo in $Modos) {
    $extra = if ($modo -eq 'off') { 'PSD_VIDEO_OFF=1 ' } else { '' }
    if ($qp -gt 0) { $extra += "PSD_MAX_QP=$qp " }
    $rot = if ($qp -gt 0 -and $modo -ne 'off') { "$modo-qp$qp" } else { $modo }
    Ssh "MPV_CLIP=`$HOME/clips/$Clip ${extra}sh ~/pistick/tools/pi/iniciar-psd.sh mpv" 60
    $pscFile = "$OutDir\$rot-psc.txt"; $cpuFile = "$OutDir\$rot-cpu.txt"; $logFile = "$OutDir\$rot-psd.txt"
    $c = Start-Process $Psc -ArgumentList @($Address, '--script', (Resolve-Path $Roteiro).Path) -PassThru -NoNewWindow -RedirectStandardOutput $pscFile -RedirectStandardError "$OutDir\$rot-psc.err"
    Start-Sleep -Seconds 11
    $cpu = Start-Process ssh -ArgumentList '-n', 'pi-stick', "sh pistick/tools/pi/medir-cpu.sh $Segundos mpv" -PassThru -NoNewWindow -RedirectStandardOutput $cpuFile -RedirectStandardError "$OutDir\$rot-cpu.err"
    if (-not $c.WaitForExit(90 * 1000)) { Stop-Process -Id $c.Id -Force; "psc não terminou ($modo)" }
    if (-not $cpu.WaitForExit(40 * 1000)) { Stop-Process -Id $cpu.Id -Force }
    $scp = Start-Process scp -ArgumentList '-q', 'pi-stick:psd.log', $logFile -PassThru -NoNewWindow
    [void]$scp.WaitForExit(30 * 1000)
    Ssh 'pkill -x psd; pkill -x mpv' 30

    $am = Get-Content $cpuFile | Where-Object { $_ -match '^\d+ \d+ \d+$' } | ForEach-Object { $p = $_ -split ' '; [pscustomobject]@{ psd = [int]$p[1]; mpv = [int]$p[2] } }
    $cpuPsd = @(); $cpuMpv = @()
    for ($i = 1; $i -lt $am.Count; $i++) { $cpuPsd += ($am[$i].psd - $am[$i - 1].psd); $cpuMpv += ($am[$i].mpv - $am[$i - 1].mpv) }
    $stats = Get-Content $pscFile -Encoding UTF8 | Where-Object { $_ -match '^VIDEOSTATS' } | Select-Object -Last 1
    $linhas = @(Get-Content $logFile -Encoding UTF8)
    $ini = ($linhas | Select-String -Pattern 'vídeo iniciado' | Select-Object -First 1)
    $de = if ($ini) { $ini.LineNumber } else { 0 }
    $enc = @($linhas | Select-Object -Skip $de | Where-Object { $_ -match 'janela \d+: (\d+) quadros.*codificação média (\d+) µs' } | ForEach-Object { [void]($_ -match 'janela \d+: (\d+) quadros.*codificação média (\d+) µs'); [int]$Matches[2] })
    $det = @($linhas | Where-Object { $_ -match 'detecção: (\d+) quadros' })
    "== psd --video $modo$(if ($qp -gt 0 -and $modo -ne 'off') { " --video-max-qp $qp" }) ($Address, clipe $Clip, ${Segundos} s)"
    $stats
    "CPU do psd: mediana {0}%  máx {1}%  média {2:N1}%  (% de um núcleo, {3} s)" -f (Med $cpuPsd), ($cpuPsd | Measure-Object -Maximum).Maximum, ($cpuPsd | Measure-Object -Average).Average, $cpuPsd.Count
    "CPU do mpv: mediana {0}%  máx {1}%  média {2:N1}%" -f (Med $cpuMpv), ($cpuMpv | Measure-Object -Maximum).Maximum, ($cpuMpv | Measure-Object -Average).Average
    "codificação por quadro (média de 1 s no log do psd, mediana de {0} linhas): {1} µs" -f $enc.Count, (Med $enc)
    "linhas de estatística do psd com a detecção (modo auto, antes do vídeo): {0}" -f $det.Count
}
}
