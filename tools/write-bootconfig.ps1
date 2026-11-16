# Escreve a configuração de primeiro boot (cloud-init) na partição "bootfs" do cartão.
# Não precisa de administrador. Pode ser rodado de novo com segurança.
param(
    [string]$Root = "C:\Users\math\Documents\Raspberry",
    [string]$HostName = "pi-stick",
    [string]$User = "pi",
    [string]$PubKey = "$env:USERPROFILE\.ssh\id_ed25519_pistick.pub",
    [string]$Country = "BR",
    [string]$TimeZone = "America/Sao_Paulo"
)
$ErrorActionPreference = 'Stop'
$vol = Get-Volume | Where-Object { $_.FileSystemLabel -eq 'bootfs' -and $_.DriveLetter } | Select-Object -First 1
if (-not $vol) { throw "volume 'bootfs' nao encontrado; reinsira o cartao SD" }
$b = "$($vol.DriveLetter):\"
if (-not (Test-Path "$b\config.txt")) { throw "$b nao tem config.txt; nao parece a particao de boot do Pi" }
$utf8 = New-Object System.Text.UTF8Encoding($false)
function WriteLf($path, $text) { [IO.File]::WriteAllText($path, ($text -replace "`r`n", "`n"), $utf8) }

$key = (Get-Content $PubKey -Raw).Trim()
$userData = @"
#cloud-config
hostname: $HostName
manage_etc_hosts: true
timezone: $TimeZone
users:
  - name: $User
    groups: users,adm,dialout,audio,netdev,video,plugdev,cdrom,games,input,gpio,spi,i2c,render,sudo
    shell: /bin/bash
    lock_passwd: true
    ssh_authorized_keys:
      - $key
    sudo: ALL=(ALL) NOPASSWD:ALL
enable_ssh: true
ssh_pwauth: false
"@
foreach ($f in 'user-data', 'network-config') { if ((Test-Path "$b$f") -and -not (Test-Path "$b$f.orig")) { Copy-Item "$b$f" "$b$f.orig" } }
WriteLf "$b\user-data" $userData
"user-data escrito (usuario $User, host $HostName, SSH so por chave)"

$wifiDir = Join-Path $Root ".cache\wifi"
$xmlFile = Get-ChildItem $wifiDir -Filter *.xml -ErrorAction SilentlyContinue | Select-Object -First 1
if ($xmlFile) {
    [xml]$x = Get-Content $xmlFile.FullName -Raw
    $ssid = $x.WLANProfile.SSIDConfig.SSID.name
    $psk = $x.WLANProfile.MSM.security.sharedKey.keyMaterial
    $esc = { param($s) $s -replace '\\', '\\' -replace '"', '\"' }
    $net = @"
network:
  version: 2
  wifis:
    renderer: NetworkManager
    wlan0:
      dhcp4: true
      regulatory-domain: "$Country"
      access-points:
        "$(& $esc $ssid)":
          password: "$(& $esc $psk)"
      optional: true
"@
    WriteLf "$b\network-config" $net
    Remove-Item $wifiDir -Recurse -Force
    "network-config escrito para o Wi-Fi '$ssid' (arquivo temporario apagado)"
} else {
    "sem perfil Wi-Fi exportado: network-config mantido; use cabo de rede"
}

# Porta USB-C em modo periférico, para o gadget USB próprio do projeto
$cfg = Get-Content "$b\config.txt" -Raw
if ($cfg -notmatch 'dr_mode=peripheral') {
    WriteLf "$b\config.txt" ($cfg.TrimEnd() + "`n`n[all]`n# pistick: porta USB-C como dispositivo (gadget)`ndtoverlay=dwc2,dr_mode=peripheral`n")
    "config.txt: dtoverlay=dwc2,dr_mode=peripheral adicionado"
}
"bootconfig OK em $b"
