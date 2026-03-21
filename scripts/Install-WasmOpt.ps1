<#
.SYNOPSIS
  Download and unpack Binaryen (wasm-opt) so SpacetimeDB can build optimized WASM modules.

.DESCRIPTION
  If wasm-opt is not on PATH, downloads the Windows x64 binaryen archive from GitHub,
  extracts it under scripts/tools/binaryen, and adds bin to PATH for this session.
  Run the benchmark in the same PowerShell session, or add the bin path to your user PATH.

.EXAMPLE
  .\Install-WasmOpt.ps1
#>
$ErrorActionPreference = "Stop"

$ScriptDir = $PSScriptRoot
$ToolsDir = Join-Path $ScriptDir "tools"
$BinaryenDir = Join-Path $ToolsDir "binaryen"
$BinDir = Join-Path $BinaryenDir "bin"

if (Get-Command wasm-opt -ErrorAction SilentlyContinue) {
    Write-Host "wasm-opt is already available." -ForegroundColor Green
    (Get-Command wasm-opt).Source
    exit 0
}

$Version = "version_128"
$ArchiveName = "binaryen-$Version-x86_64-windows.tar.gz"
$Url = "https://github.com/WebAssembly/binaryen/releases/download/$Version/$ArchiveName"
$ArchivePath = Join-Path $ToolsDir $ArchiveName

if (Test-Path (Join-Path $BinDir "wasm-opt.exe")) {
    Write-Host "Binaryen already installed at: $BinaryenDir" -ForegroundColor Green
    $env:Path = "$BinDir;$env:Path"
    Write-Host "Added to PATH for this session. Run your benchmark in this same window, or add to user PATH: $BinDir" -ForegroundColor Yellow
    exit 0
}

Write-Host "Downloading Binaryen ($ArchiveName)..." -ForegroundColor Cyan
$null = New-Item -ItemType Directory -Path $ToolsDir -Force
try {
    Invoke-WebRequest -Uri $Url -OutFile $ArchivePath -UseBasicParsing
} catch {
    Write-Error "Download failed: $_. Exception: $($_.Exception.Message)"
}

Write-Host "Extracting to $BinaryenDir ..." -ForegroundColor Cyan
$null = New-Item -ItemType Directory -Path $BinaryenDir -Force
# tar.gz on Windows: use tar if available (Windows 10+)
$tar = Get-Command tar -ErrorAction SilentlyContinue
if ($tar) {
    Push-Location $BinaryenDir
    tar -xzf $ArchivePath
    Pop-Location
} else {
    Write-Warning "No 'tar' found. Extract $ArchivePath manually to $BinaryenDir (e.g. use 7-Zip)."
    exit 1
}

# Archive extracts to e.g. binaryen-version_128/ with bin/ inside
$found = Get-ChildItem -Path $BinaryenDir -Recurse -Filter "wasm-opt.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $found) {
    Write-Error "wasm-opt.exe not found under $BinaryenDir. Check extraction."
}
$BinDir = Split-Path $found.FullName -Parent

$env:Path = "$BinDir;$env:Path"
Write-Host "Installed. wasm-opt is on PATH for this session." -ForegroundColor Green
Write-Host "To make it permanent: Add to user PATH: $BinDir" -ForegroundColor Yellow
Write-Host "Then re-run your benchmark to use optimized SpacetimeDB modules." -ForegroundColor Gray
