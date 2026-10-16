$ErrorActionPreference = "Stop"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $scriptDir

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "Installing Rust..." -ForegroundColor Yellow
    Invoke-RestMethod https://sh.rustup.rs | Invoke-Expression
    $env:PATH += ";$env:USERPROFILE\.cargo\bin"
}

if (-not $env:CHROME_BIN) {
    $paths = @(
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
        "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
    )
    foreach ($p in $paths) { if (Test-Path $p) { $env:CHROME_BIN = $p; break } }
}

$exe = Join-Path $scriptDir "target\release\cf-managed.exe"
if (-not (Test-Path $exe)) {
    Write-Host "Building cf-managed..." -ForegroundColor Cyan
    cargo build --release
    if (-not $?) { exit 1 }
}

if (Test-Path "$scriptDir\.env") {
    Get-Content "$scriptDir\.env" | ForEach-Object {
        if ($_ -match '^\s*([^#][^=]+)=(.*)$') {
            $key = $matches[1].Trim(); $val = $matches[2].Trim().Trim('"').Trim("'")
            if (-not (Get-Item "env:$key" -ErrorAction SilentlyContinue)) { Set-Item "env:$key" $val }
        }
    }
}

if (-not $env:PORT) { $env:PORT = "408" }
if (-not $env:BROWSERS) { $env:BROWSERS = "2" }
if (-not $env:TABS) { $env:TABS = "10" }
if (-not $env:timeOut) { $env:timeOut = "90000" }
if (-not $env:HEADLESS) { $env:HEADLESS = "true" }

Write-Host "Starting CF Managed Solver on port $env:PORT (BROWSERS=$env:BROWSERS TABS=$env:TABS)" -ForegroundColor Green
& $exe
