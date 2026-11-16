# Abre o psc, espera, salva um PNG de cada janela dele e encerra.
# Serve para conferir visualmente o que o cliente mostra, sem depender de alguem olhando a tela.
# Uso: tools\capture-psc.ps1 -Address 127.0.0.1:7700 [-Seconds 4] [-OutDir .cache\shot] [-ExtraArgs '--focus','1']
# Com `--script`, use um roteiro sem `quit` que termine com o que deve estar na tela (um menu aberto,
# por exemplo): a captura é das janelas do sistema, com os popups em suas posições reais.
param(
    [Parameter(Mandatory = $true)][string]$Address,
    [int]$Seconds = 4,
    [string]$OutDir = "$PSScriptRoot\..\.cache\shot",
    [string]$Psc = "$PSScriptRoot\..\target\release\psc.exe",
    [string[]]$ExtraArgs = @(),
    # Depois da espera, move a janela com título (a que não é popup) por este deslocamento (dx, dy), espera 1 s
    # e só então captura: confere que os popups acompanham a mãe.
    [int[]]$MoveBy = @()
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
Get-ChildItem $OutDir -Filter 'janela*.png' -ErrorAction SilentlyContinue | Remove-Item
$c = Start-Process $Psc -ArgumentList (@($Address) + $ExtraArgs) -PassThru -RedirectStandardOutput "$OutDir\psc.log" -RedirectStandardError "$OutDir\psc.err"
Start-Sleep -Seconds $Seconds
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Collections.Generic; using System.Text;
public class PsWin { public delegate bool EP(IntPtr h, IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EP f, IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
public static List<IntPtr> Of(uint pid) { var l = new List<IntPtr>(); EnumWindows((h, p) => { uint q; GetWindowThreadProcessId(h, out q); if (q == pid && IsWindowVisible(h)) l.Add(h); return true; }, IntPtr.Zero); return l; } }
"@
if ($MoveBy.Count -eq 2) {
    foreach ($h in [PsWin]::Of([uint32]$c.Id)) {
        $sb = New-Object System.Text.StringBuilder 300
        [PsWin]::GetWindowText($h, $sb, 300) | Out-Null
        if ($sb.Length -eq 0) { continue }   # popups não têm título
        $rc = New-Object PsWin+RECT
        [PsWin]::GetWindowRect($h, [ref]$rc) | Out-Null
        # SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE
        [PsWin]::SetWindowPos($h, [IntPtr]::Zero, $rc.L + $MoveBy[0], $rc.T + $MoveBy[1], 0, 0, 0x0001 -bor 0x0004 -bor 0x0010) | Out-Null
        "janela '$($sb.ToString())' movida de ($($rc.L), $($rc.T)) por ($($MoveBy[0]), $($MoveBy[1]))"
    }
    Start-Sleep -Seconds 1
}
$i = 0
foreach ($h in [PsWin]::Of([uint32]$c.Id)) {
    $rc = New-Object PsWin+RECT
    [PsWin]::GetWindowRect($h, [ref]$rc) | Out-Null
    $w = $rc.R - $rc.L; $hh = $rc.B - $rc.T
    if ($w -lt 50 -or $hh -lt 50) { continue }
    $sb = New-Object System.Text.StringBuilder 300
    [PsWin]::GetWindowText($h, $sb, 300) | Out-Null
    $bmp = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp); $dc = $g.GetHdc()
    [PsWin]::PrintWindow($h, $dc, 2) | Out-Null
    $g.ReleaseHdc($dc); $g.Dispose()
    $i++
    $file = Join-Path $OutDir "janela$i.png"
    $bmp.Save($file); $bmp.Dispose()
    # Origem da área de cliente na tela: é onde o servidor posiciona os popups (x, y relativos a ela).
    $pt = New-Object PsWin+POINT
    [PsWin]::ClientToScreen($h, [ref]$pt) | Out-Null
    "janela$i : '$($sb.ToString())' ${w}x$hh em ($($rc.L), $($rc.T)), área de cliente em ($($pt.X), $($pt.Y)) -> $file"
}
if ($i -eq 0) { "nenhuma janela encontrada" }
Stop-Process -Id $c.Id -Force -ErrorAction SilentlyContinue
"--- fim do log do psc"
Get-Content "$OutDir\psc.log", "$OutDir\psc.err" -Tail 4 -ErrorAction SilentlyContinue
