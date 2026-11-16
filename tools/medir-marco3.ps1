# Mede, para um roteiro do psc, a CPU do psd no Pi (amostrada por ssh a cada segundo) e o tráfego recebido
# pelo cliente (linhas "1 s:" do psc). O psd precisa estar rodando com o mousepad:
#   ssh pi-stick "sh ~/pistick/tools/pi/iniciar-psd.sh mousepad"
# Uso: tools\medir-marco3.ps1 -Roteiro tools\roteiros\51-medicao-digitando.txt -Segundos 30
param(
    [Parameter(Mandatory = $true)][string]$Roteiro,
    [int]$Segundos = 30,
    [string]$Address = '192.168.0.13:7700',
    [string]$Psc = "$PSScriptRoot\..\target\release\psc.exe",
    [string]$OutDir = "$PSScriptRoot\..\.cache\medicao"
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$nome = [IO.Path]::GetFileNameWithoutExtension($Roteiro)
$cpuFile = "$OutDir\$nome-cpu.txt"; $pscFile = "$OutDir\$nome-psc.txt"
$cpu = Start-Process ssh -ArgumentList 'pi-stick', "sh pistick/tools/pi/medir-cpu.sh $Segundos" -PassThru -NoNewWindow -RedirectStandardOutput $cpuFile
$c = Start-Process $Psc -ArgumentList @($Address, '--script', (Resolve-Path $Roteiro).Path) -PassThru -NoNewWindow -RedirectStandardOutput $pscFile -RedirectStandardError "$OutDir\$nome-psc.err"
if (-not $c.WaitForExit(($Segundos + 20) * 1000)) { Stop-Process -Id $c.Id -Force; "psc não terminou" }
$cpu.WaitForExit()

# CPU: ticks por segundo = % de um núcleo.
$amostras = Get-Content $cpuFile | Where-Object { $_ -match '^\d+ \d+ \d+$' } | ForEach-Object { $p = $_ -split ' '; [pscustomobject]@{ t = [int]$p[0]; psd = [int]$p[1]; mp = [int]$p[2] } }
$cpuPsd = @(); $cpuMp = @()
for ($i = 1; $i -lt $amostras.Count; $i++) { $cpuPsd += ($amostras[$i].psd - $amostras[$i-1].psd); $cpuMp += ($amostras[$i].mp - $amostras[$i-1].mp) }
# Tráfego: o que o psc recebeu em cada segundo.
$rx = Get-Content $pscFile -Encoding UTF8 | Where-Object { $_ -match '1 s: (\d+) quadros recebidos \(([\d.]+) KiB\)' } | ForEach-Object { [void]($_ -match '1 s: (\d+) quadros recebidos \(([\d.]+) KiB\)'); [pscustomobject]@{ q = [int]$Matches[1]; kib = [double]::Parse($Matches[2], [Globalization.CultureInfo]::InvariantCulture) } }
function Med($v) { $s = @($v | Sort-Object); if ($s.Count -eq 0) { return 0 }; $s[[int][math]::Floor(($s.Count - 1) / 2)] }
"roteiro: $nome ($($amostras.Count - 1) s de CPU, $($rx.Count) linhas do psc)"
"CPU do psd: mediana {0}%  máx {1}%  média {2:N1}%  (por segundo, % de um núcleo)" -f (Med $cpuPsd), ($cpuPsd | Measure-Object -Maximum).Maximum, ($cpuPsd | Measure-Object -Average).Average
"CPU do mousepad: mediana {0}%  máx {1}%  média {2:N1}%" -f (Med $cpuMp), ($cpuMp | Measure-Object -Maximum).Maximum, ($cpuMp | Measure-Object -Average).Average
# O primeiro segundo com quadro leva o quadro inicial inteiro das janelas: fica fora das médias de "ativo".
$ativos = @($rx | Select-Object -Skip 1 | Where-Object { $_.q -gt 0 })
"tráfego recebido: total {0:N1} KiB em {1} quadros ({2} s); 1ª linha (quadro inicial) {3:N1} KiB" -f ($rx | Measure-Object -Property kib -Sum).Sum, ($rx | Measure-Object -Property q -Sum).Sum, $rx.Count, $(if ($rx.Count) { $rx[0].kib } else { 0 })
if ($ativos.Count) {
    "segundos com quadros (sem o inicial): {0}; KiB/s mediana {1:N1}, máx {2:N1}; quadros/s mediana {3}, máx {4}" -f $ativos.Count, (Med ($ativos | ForEach-Object { $_.kib })), ($ativos | Measure-Object -Property kib -Maximum).Maximum, (Med ($ativos | ForEach-Object { $_.q })), ($ativos | Measure-Object -Property q -Maximum).Maximum
}
