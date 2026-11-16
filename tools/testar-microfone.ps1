param(
    [Parameter(Mandatory = $true)][ValidateSet('aceite2-3', 'aceite4', 'aceite6', 'latencia', 'deriva', 'real')][string]$Teste,
    [string]$Address = '192.168.0.13:7700',
    [string]$Psc = '',
    [string]$OutDir = '',
    [int]$Segundos = 5,
    [string]$PsdArgs = '',
    [switch]$NaoIniciarPsd
)
$ErrorActionPreference = 'Stop'
if ($Psc -eq '') { $Psc = Join-Path (Join-Path (Join-Path (Join-Path $PSScriptRoot '..') 'target') 'release') 'psc.exe' }
if ($OutDir -eq '') { $OutDir = Join-Path (Join-Path (Join-Path $PSScriptRoot '..') '.cache') 'mic' }
$inv = [Globalization.CultureInfo]::InvariantCulture
New-Item -ItemType Directory -Force $OutDir | Out-Null
$raiz = (Resolve-Path "$PSScriptRoot\..").Path
$Psc = (Resolve-Path $Psc).Path
$tom = Join-Path $raiz '.cache\mic-tom.wav'
$pulsos = Join-Path $raiz '.cache\mic-pulsos.wav'
Push-Location $raiz
try {
    if (-not (Test-Path $tom)) { cargo run -q -p ps-audio --example gerar_tom_wav -- $tom 2 }
    if (-not (Test-Path $pulsos)) { cargo run -q -p ps-audio --example gerar_tom_wav -- $pulsos 2 1000 1000 0.5 60 }
} finally { Pop-Location }

function Agora-Ms { [int64]([DateTime]::UtcNow - [DateTime]::UtcNow.Date).TotalMilliseconds }
function Hora-Ms($hhmmss) { $p = $hhmmss -split '[:.]'; [int64]$p[0] * 3600000 + [int64]$p[1] * 60000 + [int64]$p[2] * 1000 + [int64]$p[3] }

$psi = New-Object Diagnostics.ProcessStartInfo 'ssh'
$psi.Arguments = '-T -o ServerAliveInterval=5 pi-stick sh'
$psi.UseShellExecute = $false; $psi.RedirectStandardInput = $true; $psi.RedirectStandardOutput = $true; $psi.CreateNoWindow = $true
$shell = [Diagnostics.Process]::Start($psi)
$entrada = New-Object IO.StreamWriter($shell.StandardInput.BaseStream, (New-Object Text.UTF8Encoding($false)))
$entrada.NewLine = "`n"; $entrada.AutoFlush = $true
$entrada.WriteLine('')
function Sh([string]$cmd, [int]$seg = 60) {
    $m = 'FIM' + [guid]::NewGuid().ToString('N').Substring(0, 8)
    $entrada.WriteLine($cmd); $entrada.WriteLine("echo $m")
    $linhas = @()
    while ($true) {
        $t = $shell.StandardOutput.ReadLineAsync()
        if (-not $t.Wait($seg * 1000)) { Write-Warning "sem resposta em $seg s: $cmd"; break }
        $l = $t.Result
        if ($null -eq $l -or $l -eq $m) { break }
        $linhas += $l
    }
    $linhas
}
function Espera-Linha([string]$arq, [string]$rx, [int]$seg) {
    $fim = (Get-Date).AddSeconds($seg)
    while ((Get-Date) -lt $fim) {
        if (Test-Path $arq) {
            $l = @(Get-Content $arq -Encoding UTF8 -ErrorAction SilentlyContinue | Where-Object { $_ -match $rx })
            if ($l.Count -gt 0) { return $l[0] }
        }
        Start-Sleep -Milliseconds 100
    }
    return $null
}
function Inicia-Psc([string[]]$pscArgs, [string]$log) {
    Remove-Item $log, "$log.err" -ErrorAction SilentlyContinue
    Start-Process $Psc -ArgumentList $pscArgs -PassThru -NoNewWindow -RedirectStandardOutput $log -RedirectStandardError "$log.err"
}
function Linhas([string]$arq, [string]$rx) { @(Get-Content $arq -Encoding UTF8 -ErrorAction SilentlyContinue | Where-Object { $_ -match $rx }) }

[void](Sh 'export XDG_RUNTIME_DIR=/run/user/1000/pistick-dev; mkdir -p ~/mic-teste')
$rot = "$PSScriptRoot\roteiros"
$gravar = 'pw-record --rate 48000 --channels 2 --format s16 --target pistick-mic'
function Inicia-Psd([string]$env_ = '', [string]$apps = 'foot') {
    if ($NaoIniciarPsd) { return }
    if ($PsdArgs -eq '' -and $Teste -ne 'latencia') { $script:PsdArgs = '--mic-max-ms 150' }
    $o = Sh "$env_ PSD_AUDIO=1 PSD_ARGS='$PsdArgs' sh ~/pistick/tools/pi/iniciar-psd.sh $apps 2>&1 | tail -25" 90
    $o | ForEach-Object { "  [psd] $_" }
}
$ok = $true
function Confere([bool]$cond, [string]$texto) {
    if ($cond) { "OK      $texto" } else { "FALHOU  $texto"; $script:ok = $false }
}

switch ($Teste) {
    'aceite2-3' {
        Inicia-Psd
        $log = "$OutDir\73-psc.txt"
        $c = Inicia-Psc @($Address, '--mic-file', $tom, '--audio-null', '--script', "$rot\73-microfone.txt") $log
        $r1 = Espera-Linha $log '^MICSTATS pacotes=' 90
        "fase 1 (10 s sem gravador): $r1"
        Confere ($r1 -match 'pacotes=0 ' -and $r1 -match 'pedidos=0 ' -and $r1 -match 'captura=fechada') 'aceite 3a: sem gravador, pacotes=0 e pedidos=0 durante 10 s'
        $pid_ = @(Sh "rm -f ~/mic-teste/73.wav; $gravar ~/mic-teste/73.wav 2>/dev/null & echo `$!")[-1]
        Start-Sleep -Seconds $Segundos
        $tKill = Agora-Ms
        [void](Sh "kill -INT $pid_; sleep 1")
        if (-not $c.WaitForExit(60000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou' }
        Get-Content $log -Encoding UTF8 | Where-Object { $_ -match 'microfone|MICSTATS|roteiro' } | ForEach-Object { "  [psc] $_" }
        $off = Linhas $log 'captura DESLIGADA' | Select-Object -First 1
        if ($off -match '(\d\d:\d\d:\d\d\.\d{3})Z') {
            $atraso = (Hora-Ms $Matches[1]) - $tKill
            "gravador recebeu o sinal de saída em T; o psc desligou a captura T + $atraso ms (limite superior do atraso: o sinal ainda levou o tempo do ssh)"
            Confere ($atraso -lt 1000) "aceite 3b: pacotes param em ate 1 s depois que o gravador sai ($atraso ms)"
        } else { Confere $false 'aceite 3b: o psc registrou captura DESLIGADA' }
        $v = Sh '~/pistick/target/release/psd verify-wav ~/mic-teste/73.wav 440 880; echo codigo=$?'
        $v | ForEach-Object { "  [verify-wav] $_" }
        Confere (($v -join ' ') -match 'VERIFY wav ok' -and ($v -join ' ') -match 'codigo=0') 'aceite 2: psd verify-wav aprova a gravacao (440 Hz esq, 880 Hz dir, amplitude 0,25 +-5%)'
        $pl = Sh "grep -a 'microfone' ~/psd.log | grep -v '1 s' | tail -12"
        $pl | ForEach-Object { "  [psd.log] $_" }
    }
    'aceite4' {
        Inicia-Psd
        $log = "$OutDir\73b-psc.txt"
        $c = Inicia-Psc @($Address, '--audio-null', '--script', "$rot\73b-microfone-desligado.txt") $log
        [void](Espera-Linha $log '^MICSTATS start' 90)
        Start-Sleep -Seconds 2
        $pid_ = @(Sh "rm -f ~/mic-teste/sem-mic.wav; $gravar ~/mic-teste/sem-mic.wav 2>/dev/null & echo `$!")[-1]
        Start-Sleep -Seconds 3
        $monitores = @(Sh 'pgrep -x pw-link | wc -l; ps -C pw-record -o pid= | wc -l')
        "  com o gravador ativo no Pi: processos pw-link (monitor de ligações do psd) = $($monitores[0]), pw-record = $($monitores[1])"
        Confere ($monitores[0] -eq '0' -and $monitores[1] -eq '1') 'aceite 4: o psd nem sobe o monitor de ligações sem cliente com CAP_MIC, mesmo com gravador ativo'
        Start-Sleep -Seconds 3
        [void](Sh "kill -INT $pid_; sleep 1")
        if (-not $c.WaitForExit(60000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou' }
        Get-Content $log -Encoding UTF8 | Where-Object { $_ -match 'microfone|MICSTATS' } | ForEach-Object { "  [psc] $_" }
        $r = (Linhas $log '^MICSTATS pacotes=' | Select-Object -Last 1)
        Confere ($r -match 'pacotes=0 ' -and $r -match 'pedidos=0 ' -and $r -match 'anunciou=0 ') "aceite 4: o psc não anunciou nem recebeu pedido ($r)"
        $pico = @(Sh 'python3 ~/pistick/tools/pi/pico-wav.py ~/mic-teste/sem-mic.wav')[-1]
        "  gravador do Pi: $pico"
        Confere ("$pico" -match 'pico 0$' -and "$pico" -match 'quadros [1-9]') 'aceite 4: o gravador do Pi recebeu so silencio (pico 0)'
        $pl = Sh "grep -a 'microfone' ~/psd.log | grep -v '1 s' | tail -8"
        $pl | ForEach-Object { "  [psd.log] $_" }
        Confere (-not (($pl -join ' ') -match 'pedido ligado')) 'aceite 4: o psd nao mandou pedido'
    }
    'aceite6' {
        Inicia-Psd 'PSD_AUDIO_RT=1 MPV_CLIP=$HOME/clips/som-720p30.mp4' 'mpv'
        $log = "$OutDir\74-psc.txt"
        $c = Inicia-Psc @($Address, '--mic-file', $tom, '--audio-null', '--script', "$rot\74-microfone-e-som.txt") $log
        [void](Espera-Linha $log '^MICSTATS start' 120)
        Start-Sleep -Seconds 1
        $pid_ = @(Sh "rm -f ~/mic-teste/74.wav; $gravar ~/mic-teste/74.wav 2>/dev/null & echo `$!")[-1]
        Start-Sleep -Seconds $Segundos
        [void](Sh "kill -INT $pid_; sleep 1")
        if (-not $c.WaitForExit(60000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou' }
        Get-Content $log -Encoding UTF8 | Where-Object { $_ -match 'VERIFY|AUDIOSTATS|MICSTATS|waitmic|roteiro' } | ForEach-Object { "  [psc] $_" }
        $v = Sh '~/pistick/target/release/psd verify-wav ~/mic-teste/74.wav 440 880; echo codigo=$?'
        $v | ForEach-Object { "  [verify-wav] $_" }
        (Sh "grep -a 'microfone: [0-9]* pacotes' ~/psd.log | tail -4") | ForEach-Object { "  [psd.log] $_" }
        Confere ((Linhas $log '^VERIFY audio ok').Count -ge 1) 'aceite 6: verify-audio da saida passou enquanto o microfone gravava'
        Confere (($v -join ' ') -match 'VERIFY wav ok') 'aceite 6: o microfone gravado continua correto com o som tocando'
        if ((Linhas $log 'FALHOU').Count -gt 0) { $script:ok = $false }
    }
    'latencia' {
        $melhor = $null
        1..40 | ForEach-Object {
            $t0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            $pi_ = [double]@(Sh 'date +%s.%N')[0]
            $t1 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            if ($null -eq $melhor -or ($t1 - $t0) -lt $melhor.rtt) { $melhor = @{ rtt = $t1 - $t0; off = $pi_ * 1000 - ($t0 + $t1) / 2 } }
        }
        "relógios: Pi - Windows = {0:N1} ms (menor ida e volta do ssh {1} ms)" -f $melhor.off, $melhor.rtt
        Inicia-Psd
        $log = "$OutDir\76-psc.txt"
        $env:PSC_MIC_TRACE = '1'
        $c = Inicia-Psc @($Address, '--mic-file', $pulsos, '--audio-null', '--script', "$rot\76-microfone-atraso.txt") $log
        Remove-Item Env:\PSC_MIC_TRACE
        [void](Espera-Linha $log 'roteiro|janela' 90)
        $dur = [math]::Max($Segundos, 45)
        [void](Sh "rm -f ~/mic-teste/onsets.txt; (python3 ~/pistick/tools/pi/atraso-microfone.py $dur > ~/mic-teste/onsets.txt 2>/dev/null &)")
        Start-Sleep -Seconds ($dur + 2)
        if (-not $c.WaitForExit(90000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou (o gravador saiu; ele desliga em 0,5 s)' }
        $traces = Linhas $log 'trace' | ForEach-Object { if ($_ -match '(\d\d:\d\d:\d\d\.\d{3})Z') { (Hora-Ms $Matches[1]) } }
        $ons = Sh 'cat ~/mic-teste/onsets.txt'
        $pis = $ons | Where-Object { $_ -match '^ONSET ([\d.]+)' } | ForEach-Object { [void]($_ -match '^ONSET ([\d.]+)'); $s = [double]::Parse($Matches[1], $inv) * 1000 - $melhor.off; [int64]($s % 86400000) }
        "pulsos enviados pelo psc: $(@($traces).Count); vistos no Pi: $(@($pis).Count)"
        $lat = @()
        foreach ($p in $pis) {
            $ant = @($traces | Where-Object { $_ -le $p + 5 -and $_ -ge $p - 1500 }) | Select-Object -Last 1
            if ($null -ne $ant) { $lat += ($p - $ant) }
        }
        function Estat($v, $nome) {
            $o = @($v | Sort-Object)
            if ($o.Count -eq 0) { return $null }
            $med = $o[[int][math]::Floor(($o.Count - 1) / 2)]
            Write-Host ("{0} mediana={1} min={2} max={3} n={4}" -f $nome, $med, $o[0], $o[-1], $o.Count)
            return $med
        }
        "atraso (ms) por pulso, em ordem: " + ($lat -join ' ')
        [void](Estat $lat 'ATRASO (todos os pulsos, com o aquecimento de 6 s da fila)')
        $regime = @($lat | Select-Object -Skip 20)
        $medR = Estat $regime 'ATRASO (em regime, depois de 20 s)'
        "(incerteza dos relógios: +-$([math]::Round($melhor.rtt / 2)) ms)"
        if ($null -ne $medR) { Confere ($medR -lt 150) "aceite 5: atraso mediano em regime abaixo de 150 ms ($medR ms)" } else { Confere $false 'aceite 5: poucos pulsos para medir em regime (use -Segundos 45 ou mais)' }
        $pl = Sh "grep -a 'microfone' ~/psd.log | grep ' 1 s\|pacotes' | tail -3"
        $pl | ForEach-Object { "  [psd.log] $_" }
    }
    'deriva' {
        Inicia-Psd 'PSD_AUDIO_RT=1'
        $log = "$OutDir\77-psc.txt"
        $c = Inicia-Psc @($Address, '--mic-file', $tom, '--audio-null', '--script', "$rot\77-microfone-deriva.txt") $log
        [void](Espera-Linha $log '^MICSTATS start' 90)
        Start-Sleep -Seconds 1
        $t0 = Agora-Ms
        $pid_ = @(Sh "rm -f ~/mic-teste/deriva.wav; $gravar ~/mic-teste/deriva.wav 2>/dev/null & echo `$!")[-1]
        Start-Sleep -Seconds $Segundos
        $t1 = Agora-Ms
        [void](Sh "kill -INT $pid_; sleep 1")
        if (-not $c.WaitForExit(120000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou' }
        "gravação de {0:N1} s (relógio do Windows)" -f (($t1 - $t0) / 1000)
        $v = Sh 'python3 ~/pistick/tools/pi/analisa-deriva.py ~/mic-teste/deriva.wav 440' 400
        $v | ForEach-Object { "  [analise] $_" }
        $dur = ($v | Where-Object { $_ -match 'duracao ([\d.]+) s' } | ForEach-Object { [double]::Parse($Matches[1], $inv) } | Select-Object -First 1)
        $bur = ($v | Where-Object { $_ -match 'buracos\): (\d+) de (\d+)' } | ForEach-Object { [pscustomobject]@{ n = [int]$Matches[1]; de = [int]$Matches[2] } } | Select-Object -First 1)
        $reg = ($v | Where-Object { $_ -match 'SALTOS_REGIME.*eventos=(\d+) soma=(-?[\d.]+)' } | ForEach-Object { [pscustomobject]@{ ev = [int]$Matches[1]; soma = [double]::Parse($Matches[2], $inv) } } | Select-Object -First 1)
        Confere ($null -ne $dur -and [math]::Abs($dur - ($t1 - $t0) / 1000) -lt 1.5) "deriva: a gravacao dura o mesmo que o relogio ($dur s contra $(($t1 - $t0) / 1000) s): nada se acumula"
        Confere ($null -ne $reg -and [math]::Abs($reg.soma) -lt 240) "deriva: depois dos 30 s do começo a soma dos saltos de fase e de menos de 5 ms (soma $($reg.soma) quadros, $($reg.ev) eventos): sem deriva acumulada"
        Confere ($null -ne $bur -and $bur.n * 1000 -le $bur.de) "deriva: no maximo 0,1% dos blocos de 10 ms com pico baixo ($($bur.n) de $($bur.de))"
        Get-Content $log -Encoding UTF8 | Where-Object { $_ -match 'MICSTATS' } | ForEach-Object { "  [psc] $_" }
        $pl = Sh "grep -a 'microfone: [0-9]* pacotes' ~/psd.log | tail -3; grep -a -c 'conexão com o PipeWire caiu' ~/psd.log"
        $pl | ForEach-Object { "  [psd.log] $_" }
    }
    'real' {
        Inicia-Psd
        $log = "$OutDir\75-psc.txt"
        $c = Inicia-Psc @($Address, '--mic', '--audio-null', '--script', "$rot\75-microfone-real.txt") $log
        [void](Espera-Linha $log '^MICSTATS start' 90)
        Start-Sleep -Seconds 1
        $pid_ = @(Sh "$gravar /dev/null 2>/dev/null & echo `$!")[-1]
        Start-Sleep -Seconds 8
        [void](Sh "kill -INT $pid_; sleep 1")
        if (-not $c.WaitForExit(60000)) { Stop-Process -Id $c.Id -Force; 'psc não terminou' }
        Get-Content $log -Encoding UTF8 | Where-Object { $_ -match 'microfone|MICSTATS|waitmic' } | ForEach-Object { "  [psc] $_" }
        $r = (Linhas $log '^MICSTATS pacotes=' | Select-Object -First 1)
        Confere ($r -match 'pacotes=([1-9]\d*) ') 'aceite 8: o dispositivo abriu e chegaram amostras (pacotes > 0)'
        if ($r -match 'pico=(\d+)') { "  nível: pico=$($Matches[1]) ($(if ([int]$Matches[1] -eq 0) { 'silêncio total' } else { 'houve som' }))" }
    }
}

$entrada.WriteLine('exit')
if (-not $shell.HasExited) { $shell.Kill() }
if ($ok) { "RESULTADO: passou" } else { "RESULTADO: FALHOU"; exit 3 }
