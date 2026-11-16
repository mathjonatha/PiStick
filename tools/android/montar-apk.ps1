

[CmdletBinding()]
param(
    [string]$Sdk,
    [string]$Ndk = '27.1.12297006',
    [string]$BuildTools = '36.0.0',
    [string]$Plataforma = 'android-36',
    [string]$Jdk,
    [string]$Keystore,
    [string]$Saida,
    [ValidateSet('release', 'debug')][string]$Perfil = 'release'
)

$ErrorActionPreference = 'Stop'
$Raiz = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $Sdk) { $Sdk = 'C:\Users\math\Documents\02. REA\android-sdk' }
if (-not $Jdk) { $Jdk = $env:JAVA_HOME }
if (-not $Keystore) { $Keystore = Join-Path $env:USERPROFILE '.android\debug.keystore' }
if (-not $Saida) { $Saida = Join-Path $Raiz 'target\android' }

$Alvo = 'aarch64-linux-android'
$Api = 30
$Biblioteca = 'libps_android.so'

function Falha([string]$msg) {
    Write-Host "ERRO: $msg" -ForegroundColor Red
    exit 1
}

function Exige([string]$caminho, [string]$oque, [string]$dica) {
    if (-not (Test-Path -LiteralPath $caminho)) { Falha "$oque nao encontrado em: $caminho`n  $dica" }
    return $caminho
}

function Executa([string]$exe, [string[]]$argumentos, [string]$rotulo) {
    & $exe @argumentos
    if ($LASTEXITCODE -ne 0) { Falha "$rotulo falhou (codigo $LASTEXITCODE)." }
}

$bt = Exige (Join-Path $Sdk "build-tools\$BuildTools") "build-tools $BuildTools" "Parametro -Sdk ($Sdk) e -BuildTools; o roteiro nao instala nada."
$aapt2 = Exige (Join-Path $bt 'aapt2.exe') 'aapt2' 'Build-tools incompletas.'
$d8 = Exige (Join-Path $bt 'd8.bat') 'd8' 'Build-tools incompletas.'
$zipalign = Exige (Join-Path $bt 'zipalign.exe') 'zipalign' 'Build-tools incompletas.'
$apksigner = Exige (Join-Path $bt 'apksigner.bat') 'apksigner' 'Build-tools incompletas.'
$androidJar = Exige (Join-Path $Sdk "platforms\$Plataforma\android.jar") "android.jar da plataforma $Plataforma" 'Parametro -Plataforma; o roteiro nao instala nada.'
$ndkDir = Exige (Join-Path $Sdk "ndk\$Ndk") "NDK $Ndk" 'Parametro -Ndk; o roteiro nao instala nada.'
$ndkBin = Join-Path $ndkDir 'toolchains\llvm\prebuilt\windows-x86_64\bin'
$clang = Exige (Join-Path $ndkBin "aarch64-linux-android$Api-clang.cmd") 'ligador clang do NDK' "NDK $Ndk sem o prefixo da API $Api."
$llvmAr = Join-Path $ndkBin 'llvm-ar.exe'
if (-not $Jdk) { Falha 'JDK nao encontrado: defina JAVA_HOME ou passe -Jdk.' }
$javac = Exige (Join-Path $Jdk 'bin\javac.exe') 'javac' 'Parametro -Jdk ou JAVA_HOME (JDK 21).'
$keytool = Exige (Join-Path $Jdk 'bin\keytool.exe') 'keytool' 'Parametro -Jdk ou JAVA_HOME (JDK 21).'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Falha 'cargo nao encontrado no PATH.' }
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { Falha 'rustup nao encontrado no PATH.' }
$instalados = (& rustup target list --installed) -join "`n"
if ($instalados -notmatch [regex]::Escape($Alvo)) { Falha "O alvo do Rust $Alvo nao esta instalado. Rode uma vez: rustup target add $Alvo" }

$Manifesto = Exige (Join-Path $Raiz 'android\AndroidManifest.xml') 'manifesto' 'Arvore do repositorio incompleta.'
$FonteJava = Exige (Join-Path $Raiz 'android\java') 'fontes Java' 'Arvore do repositorio incompleta.'
$versao = ([regex]::Match((Get-Content -Raw (Join-Path $Raiz 'Cargo.toml')), '(?m)^version\s*=\s*"([^"]+)"')).Groups[1].Value
if (-not $versao) { Falha 'Nao achei a versao em Cargo.toml.' }

$obj = Join-Path $Saida 'obj'
if (Test-Path -LiteralPath $obj) { Remove-Item -LiteralPath $obj -Recurse -Force }
New-Item -ItemType Directory -Force -Path $obj | Out-Null
$classes = Join-Path $obj 'classes'
$dex = Join-Path $obj 'dex'
New-Item -ItemType Directory -Force -Path $classes, $dex | Out-Null
$apkFinal = Join-Path $Saida 'pistick-cliente.apk'
if (Test-Path -LiteralPath $apkFinal) { Remove-Item -LiteralPath $apkFinal -Force }

Write-Host "== 1/6 cargo build ($Alvo, $Perfil)"
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $clang
$env:CC_aarch64_linux_android = $clang
if (Test-Path -LiteralPath $llvmAr) { $env:AR_aarch64_linux_android = $llvmAr }
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS = '-C link-arg=-Wl,-z,max-page-size=16384'
$cargoArgs = @('build', '-p', 'ps-android', '--target', $Alvo)
if ($Perfil -eq 'release') { $cargoArgs += '--release' }
Push-Location $Raiz
try { Executa 'cargo' $cargoArgs 'cargo build' } finally { Pop-Location }
$so = Exige (Join-Path $Raiz "target\$Alvo\$Perfil\$Biblioteca") $Biblioteca 'O cargo build nao gerou a biblioteca.'

Write-Host '== 2/6 aapt2 link'
$base = Join-Path $obj 'base.apk'
Executa $aapt2 @('link', '-o', $base, '--manifest', $Manifesto, '-I', $androidJar, '--min-sdk-version', "$Api", '--target-sdk-version', '36',
    '--version-code', '1', '--version-name', $versao, '--debug-mode') 'aapt2 link'

Write-Host '== 3/6 javac'
$fontes = @(Get-ChildItem -LiteralPath $FonteJava -Recurse -Filter *.java | ForEach-Object { $_.FullName })
if ($fontes.Count -eq 0) { Falha "Nenhum .java em $FonteJava." }
Executa $javac (@('-encoding', 'UTF-8', '--release', '11', '-Xlint:all', '-classpath', $androidJar, '-d', $classes) + $fontes) 'javac'

Write-Host '== 4/6 d8'
$arquivosClass = @(Get-ChildItem -LiteralPath $classes -Recurse -Filter *.class | ForEach-Object { $_.FullName })
if ($arquivosClass.Count -eq 0) { Falha 'O javac nao gerou classes.' }
$env:JAVA_HOME = $Jdk
Executa $d8 (@('--lib', $androidJar, '--min-api', "$Api", '--output', $dex) + $arquivosClass) 'd8'
$classesDex = Exige (Join-Path $dex 'classes.dex') 'classes.dex' 'O d8 nao gerou o dex.'

Write-Host '== 5/6 montagem, zipalign'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$naoAlinhado = Join-Path $obj 'nao-alinhado.apk'
Copy-Item -LiteralPath $base -Destination $naoAlinhado
$zip = [System.IO.Compression.ZipFile]::Open($naoAlinhado, [System.IO.Compression.ZipArchiveMode]::Update)
try {
    [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $classesDex, 'classes.dex', [System.IO.Compression.CompressionLevel]::Optimal)
    [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $so, "lib/arm64-v8a/$Biblioteca", [System.IO.Compression.CompressionLevel]::Optimal)
} finally { $zip.Dispose() }
$alinhado = Join-Path $obj 'alinhado.apk'
Executa $zipalign @('-f', '4', $naoAlinhado, $alinhado) 'zipalign'

Write-Host '== 6/6 apksigner'
$senha = $env:PISTICK_KS_PASS
if (-not $senha) { $senha = 'android' }
$env:PISTICK_KS_SENHA_INTERNA = $senha
try {
    $guardaRaiz = [System.IO.Path]::GetFullPath($Raiz).TrimEnd('\') + '\'
    if ([System.IO.Path]::GetFullPath($Keystore).StartsWith($guardaRaiz, [System.StringComparison]::OrdinalIgnoreCase)) {
        Falha "O repositorio de chaves nao pode ficar dentro do repositorio git ($Raiz)."
    }
    if (-not (Test-Path -LiteralPath $Keystore)) {
        Write-Host "(criando o repositorio de chaves de depuracao, fora do repositorio git: $Keystore)"
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Keystore) | Out-Null
        Executa $keytool @('-genkeypair', '-keystore', $Keystore, '-storepass:env', 'PISTICK_KS_SENHA_INTERNA', '-keypass:env', 'PISTICK_KS_SENHA_INTERNA',
            '-alias', 'androiddebugkey', '-dname', 'CN=Android Debug,O=Android,C=US', '-keyalg', 'RSA', '-keysize', '2048', '-validity', '10000') 'keytool'
    }
    Executa $apksigner @('sign', '--ks', $Keystore, '--ks-pass', 'env:PISTICK_KS_SENHA_INTERNA', '--key-pass', 'env:PISTICK_KS_SENHA_INTERNA',
        '--out', $apkFinal, $alinhado) 'apksigner sign (se a senha do repositorio de chaves nao e a padrao do Android, defina PISTICK_KS_PASS)'
} finally {
    Remove-Item Env:\PISTICK_KS_SENHA_INTERNA -ErrorAction SilentlyContinue
}
Executa $apksigner @('verify', $apkFinal) 'apksigner verify'

$tam = (Get-Item -LiteralPath $apkFinal).Length
Write-Host ('OK: {0} ({1:N0} bytes, {2:N1} KiB)' -f $apkFinal, $tam, ($tam / 1KB))
