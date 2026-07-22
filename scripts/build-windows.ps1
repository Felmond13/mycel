# Build a self-contained myc.exe: Linux twin built in WSL, embedded into the
# Windows binary (see docs/desktop-architecture.md). PowerShell 5.1 compatible.
#
#   powershell -File scripts\build-windows.ps1 [-Distro Ubuntu] [-OutDir C:\mycel-win]
param(
    [string]$Distro = "Ubuntu",
    [string]$OutDir = "$env:USERPROFILE\mycel-win"
)
$ErrorActionPreference = "Stop"
$repoWin = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

# 1. Build the Linux twin inside WSL (repo may live in WSL or on Windows).
$repoWsl = (wsl -d $Distro --exec wslpath -u "$repoWin").Trim()
wsl -d $Distro -- bash "$repoWsl/scripts/dev.sh" release
if ($LASTEXITCODE -ne 0) { throw "Linux twin build failed" }

# 2. Stage the twin where the Windows build can embed it.
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Copy-Item "$repoWin\target\release\myc" "$OutDir\myc-linux" -Force

# 3. Build myc.exe with the twin embedded.
$env:MYCEL_LINUX_BINARY = "$OutDir\myc-linux"
Push-Location $repoWin
try {
    cargo build --release -p myc-cli
    if ($LASTEXITCODE -ne 0) { throw "Windows build failed" }
}
finally { Pop-Location }

Copy-Item "$repoWin\target\release\myc.exe" "$OutDir\myc.exe" -Force
$size = (Get-Item "$OutDir\myc.exe").Length
Write-Host ("built {0} ({1:N1} MB, twin embedded)" -f "$OutDir\myc.exe", ($size / 1MB))
