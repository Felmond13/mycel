# End-to-end test of myc.exe on a real Windows host with WSL2.
# Exercises the transparent WSL2 proxy: doctor, store listing, container
# run, registry ingest, and the web UI (served inside WSL, reached from
# Windows via localhost forwarding). PowerShell 5.1 compatible.
#
#   powershell -File scripts\e2e-windows.ps1 [-Myc C:\mycel-win\myc.exe] [-UiPort 7778]
param(
    [string]$Myc = "$env:USERPROFILE\mycel-win\myc.exe",
    [int]$UiPort = 7778
)
$ErrorActionPreference = "Stop"
$failures = 0

function Step([string]$name, [scriptblock]$body) {
    Write-Host "== $name" -ForegroundColor Cyan
    try {
        & $body
        Write-Host "PASS $name" -ForegroundColor Green
    }
    catch {
        Write-Host "FAIL $name : $_" -ForegroundColor Red
        $script:failures++
    }
}

if (-not (Test-Path $Myc)) { throw "myc.exe not found at $Myc (build it with scripts\build-windows.ps1)" }

Step "myc.exe with no args from a shell (welcome text, no dashboard)" {
    # From an interactive shell the console is shared, so the double-click
    # detection (GetConsoleProcessList == 1) must NOT trigger: bare `myc`
    # keeps printing the guided welcome and exits immediately.
    $out = & $Myc 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "bare myc exited $LASTEXITCODE" }
    if ($out -notmatch "content-addressed container runtime") { throw "welcome text missing" }
    if ($out -match "Starting Mycel dashboard") { throw "double-click path triggered from a shell" }
}

Step "myc.exe doctor (WSL detected, twin provisioned)" {
    & $Myc doctor
    if ($LASTEXITCODE -ne 0) { throw "doctor exited $LASTEXITCODE" }
}

Step "myc.exe ls (lists the WSL store)" {
    $out = & $Myc ls 2>&1 | Out-String
    Write-Host $out
    if ($LASTEXITCODE -ne 0) { throw "ls exited $LASTEXITCODE" }
}

Step "myc.exe run alpine:3.20 -- /bin/echo hello-from-windows" {
    $out = & $Myc run alpine:3.20 -- /bin/echo hello-from-windows 2>&1 | Out-String
    Write-Host $out
    if ($LASTEXITCODE -ne 0) { throw "run exited $LASTEXITCODE" }
    if ($out -notmatch "hello-from-windows") { throw "expected container output missing" }
}

Step "myc.exe ingest busybox:latest" {
    & $Myc ingest busybox:latest
    if ($LASTEXITCODE -ne 0) { throw "ingest exited $LASTEXITCODE" }
}

Step "myc.exe ui --port $UiPort reachable from Windows" {
    $outLog = Join-Path $env:TEMP "myc-ui-e2e-out.txt"
    $errLog = Join-Path $env:TEMP "myc-ui-e2e-err.txt"
    $proc = Start-Process -FilePath $Myc -ArgumentList "ui", "--port", "$UiPort" -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    try {
        $ok = $false
        foreach ($i in 1..45) {
            Start-Sleep -Seconds 1
            if ($proc.HasExited) { break }
            try {
                $resp = Invoke-WebRequest -UseBasicParsing "http://localhost:$UiPort/api/stats" -TimeoutSec 3
                if ($resp.StatusCode -eq 200) { $ok = $true; break }
            }
            catch { }
        }
        if (-not $ok) {
            Get-Content $outLog, $errLog -ErrorAction SilentlyContinue | Write-Host
            throw "http://localhost:$UiPort/api/stats never returned 200"
        }
        Write-Host "GET /api/stats -> 200: $($resp.Content)"
    }
    finally {
        # Stop the proxy process; the server inside WSL dies with its console.
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
        # Belt and braces: kill any leftover twin UI server inside WSL
        # ([u] avoids pkill matching this very command line).
        wsl --exec sh -c "pkill -f 'mycel-bin/myc [u]i' 2>/dev/null; true" | Out-Null
    }
}

Write-Host ""
if ($failures -eq 0) {
    Write-Host "e2e-windows: all steps passed" -ForegroundColor Green
    exit 0
}
else {
    Write-Host "e2e-windows: $failures step(s) failed" -ForegroundColor Red
    exit 1
}
