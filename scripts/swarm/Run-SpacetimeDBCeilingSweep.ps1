<#
.SYNOPSIS
    Find SpacetimeDB ceiling with server-physics: run arcane-swarm-sdk at increasing player counts.

.DESCRIPTION
    Run from arcane-scaling-benchmarks repo (clone with --recurse-submodules or git submodule update --init).
    Prerequisites: run "spacetime start" in another terminal first.
    Builds/publishes module from arcane-demos submodule, builds arcane-swarm-sdk, runs sweep.
#>
param(
    [int[]] $PlayerCounts = @(100, 150, 200),
    [switch] $FindCeiling,
    [int]   $Step = 250,
    [int]   $MaxPlayers = 2000,
    [int]   $Duration = 30,
    [double] $MaxErrRate = 0.01,
    [double] $MaxLatencyMs = 200,
    [switch] $NoPublish,
    [string] $OutCsv = "",
    [int]   $RepeatCount = 1,
    [int]   $CooldownSeconds = 0,
    [string] $SpacetimeHost = "http://127.0.0.1:3000",
    [string] $DatabaseName = "arcane"
)

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot
$BenchmarkRoot = Resolve-Path (Join-Path $ScriptDir "..\..")
#
# Benchmark runtime is vendored into this repo:
# - swarm binary: crates/arcane-benchmark-swarm
# - SpacetimeDB module source: spacetimedb_demo/spacetimedb
#
$ModulePath = Join-Path $BenchmarkRoot "spacetimedb_demo\spacetimedb"
$Exe = Join-Path $BenchmarkRoot "crates\arcane-benchmark-swarm\target\release\arcane-swarm.exe"
$SwarmCrateRoot = Join-Path $BenchmarkRoot "crates\arcane-benchmark-swarm"
if ($OutCsv -eq "") { $OutCsv = Join-Path $ScriptDir "spacetimedb_ceiling_sweep.csv" }

# Canonical parameters (must match Arcane+Spacetime runs for comparable ceilings)
$CanonicalTickRateHz = 10
$CanonicalAPS = 2
$CanonicalDurationSec = 30
$CanonicalMode = "spread"
$CanonicalServerPhysics = $true
$CanonicalBackend = "spacetimedb_only"
$CanonicalDemoEntities = 0

# Resolve SpacetimeDB CLI
$SpacetimeAvailable = $false
if (Get-Command spacetime -ErrorAction SilentlyContinue) {
    $SpacetimeAvailable = $true
} else {
    $spacetimeBase = Join-Path $env:LocalAppData "SpacetimeDB"
    foreach ($candidate in @("spacetime.exe", "bin\spacetime.exe")) {
        $p = Join-Path $spacetimeBase $candidate
        if (Test-Path $p) {
            $cliDir = Split-Path $p -Parent
            $env:Path = "$cliDir;" + $env:Path
            Write-Host "Added to PATH: $cliDir" -ForegroundColor Gray
            $SpacetimeAvailable = $true
            break
        }
    }
}
if (-not $SpacetimeAvailable) {
    Write-Warning "SpacetimeDB CLI not found (PATH or $env:LocalAppData\SpacetimeDB). Install: iwr https://windows.spacetimedb.com -useb | iex"
}

# Build and publish module (unless -NoPublish)
if (-not $NoPublish) {
    if (-not $SpacetimeAvailable) { throw "Cannot build/publish: SpacetimeDB CLI not found." }
    Write-Host "Building SpacetimeDB module (server_physics default)..." -ForegroundColor Yellow
    Push-Location $ModulePath
    try {
        cmd /c "spacetime build 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime build failed" }
        cmd /c "spacetime publish $DatabaseName --yes 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime publish failed. Is 'spacetime start' running?" }
    } finally { Pop-Location }
    Write-Host "Module published to $SpacetimeHost / $DatabaseName" -ForegroundColor Green
}

# Build arcane-swarm (vendored in this benchmark repo)
if (-not (Test-Path $Exe)) {
    Write-Host "Building arcane-swarm (release)..." -ForegroundColor Yellow
    Push-Location $SwarmCrateRoot
    cmd /c "cargo build --bin arcane-swarm --release 2>&1"
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "Build failed" }
    Pop-Location
}

function Parse-FinalLine {
    param([string] $Content)
    if ($Content -match "FINAL:\s*players=(\d+)\s+total_calls=(\d+)\s+total_oks=(\d+)\s+total_errs=(\d+)\s+lat_avg_ms=([\d.]+)") {
        return [PSCustomObject]@{
            players = [int]$Matches[1]
            total_calls = [long]$Matches[2]
            total_oks = [long]$Matches[3]
            total_errs = [long]$Matches[4]
            lat_avg_ms = [double]$Matches[5]
        }
    }
    return $null
}

function Test-Pass {
    param($Parsed, [double]$MaxErrRate, [double]$MaxLatencyMs)
    if (-not $Parsed) { return $false }
    $errRate = if ($Parsed.total_calls -gt 0) { $Parsed.total_errs / $Parsed.total_calls } else { 1.0 }
    return ($errRate -lt $MaxErrRate -and $Parsed.lat_avg_ms -lt $MaxLatencyMs)
}

function Get-Median {
    param([double[]] $Values)
    if ($Values.Count -eq 0) { return 0.0 }
    $sorted = $Values | Sort-Object
    $mid = [math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2 -eq 1) { return $sorted[$mid] }
    return ($sorted[$mid - 1] + $sorted[$mid]) / 2.0
}

$runs = [System.Collections.Generic.List[int]]::new()
if ($FindCeiling) {
    $N = $Step
    while ($N -le $MaxPlayers) { $runs.Add($N); $N += $Step }
} else {
    foreach ($p in $PlayerCounts) { $runs.Add($p) }
}

$existingN = @()
$initialCeiling = $null
if (Test-Path $OutCsv) {
    $existing = Import-Csv -Path $OutCsv -ErrorAction SilentlyContinue
    if ($existing) {
        $existingN = $existing | ForEach-Object { [int]$_.players } | Sort-Object -Unique
        $passed = $existing | Where-Object { $_.pass -eq 'True' }
        if ($passed) {
            $initialCeiling = $passed | ForEach-Object { [int]$_.players } | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum
        }
    }
}
$runsFiltered = [System.Collections.Generic.List[int]]::new()
foreach ($n in $runs) { if ($n -notin $existingN) { $runsFiltered.Add($n) } }
$runs = $runsFiltered

$results = [System.Collections.Generic.List[object]]::new()
$ceiling = $initialCeiling

Write-Host "`n=== SpacetimeDB ceiling sweep (server-physics) ===" -ForegroundColor Cyan
Write-Host "--- Canonical run parameters (fixed for all experiments) ---" -ForegroundColor Yellow
Write-Host "  tick_rate_hz=$CanonicalTickRateHz  aps=$CanonicalAPS  duration_s=$CanonicalDurationSec  mode=$CanonicalMode  server_physics=$CanonicalServerPhysics  backend=$CanonicalBackend  demo_entities=$CanonicalDemoEntities"
Write-Host "  visibility=everyone_sees_everyone  pass_criteria=err_rate<$MaxErrRate lat_avg_ms<$MaxLatencyMs"
Write-Host "---" -ForegroundColor Yellow
if ($existingN.Count -gt 0) {
    Write-Host "  Skipping N already in CSV: $($existingN -join ', ')" -ForegroundColor Gray
}
Write-Host "  Host: $SpacetimeHost  DB: $DatabaseName  Runs: $($runs.Count)  Duration: ${CanonicalDurationSec}s  RepeatCount: $RepeatCount  Cooldown: ${CooldownSeconds}s  OutCsv: $OutCsv`n"

if ($runs.Count -eq 0) {
    Write-Host "  No new runs (all planned N already have results)." -ForegroundColor Yellow
    if ($null -ne $ceiling) { Write-Host "  Current ceiling from CSV: N=$ceiling" -ForegroundColor Cyan }
    Write-Host "Done."
    exit 0
}

foreach ($N in $runs) {
    $latencies = [System.Collections.Generic.List[double]]::new()
    $allPassed = $true
    $lastParsed = $null
    for ($r = 1; $r -le $RepeatCount; $r++) {
        if ($RepeatCount -gt 1) { Write-Host "[N=$N] Rep $r/$RepeatCount..." -NoNewline } else { Write-Host "[N=$N] Running..." -NoNewline }
        $tmpOut = [System.IO.Path]::GetTempFileName()
        $tmpErr = [System.IO.Path]::GetTempFileName()
        $proc = Start-Process -FilePath $Exe -ArgumentList @(
            "--players", $N,
            "--tick-rate", $CanonicalTickRateHz,
            "--aps", $CanonicalAPS,
            "--duration", $CanonicalDurationSec,
            "--mode", $CanonicalMode,
            "--server-physics",
            "--uri", $SpacetimeHost,
            "--db", $DatabaseName
        ) -WorkingDirectory $BenchmarkRoot -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr -Wait -NoNewWindow -PassThru
        $out = Get-Content -Path $tmpOut -Raw -ErrorAction SilentlyContinue
        $err = Get-Content -Path $tmpErr -Raw -ErrorAction SilentlyContinue
        $all = if ($out) { $out } else { "" }; if ($err) { $all += "`n" + $err }
        Remove-Item -Path $tmpOut, $tmpErr -Force -ErrorAction SilentlyContinue
        $parsed = Parse-FinalLine $all
        $lastParsed = $parsed
        if ($parsed) { $latencies.Add($parsed.lat_avg_ms) }
        $runPass = Test-Pass $parsed $MaxErrRate $MaxLatencyMs
        if (-not $runPass) { $allPassed = $false }
        $lat = if ($parsed) { $parsed.lat_avg_ms } else { 0.0 }
        Write-Host " lat_avg_ms=$([math]::Round($lat,1))$(if ($runPass) { ' pass' } else { ' fail' })"
        if ($CooldownSeconds -gt 0 -and $r -lt $RepeatCount) { Start-Sleep -Seconds $CooldownSeconds }
    }
    $lat_avg_ms = Get-Median -Values $latencies
    $pass = $allPassed -and ($lat_avg_ms -lt $MaxLatencyMs)
    if ($pass) { $ceiling = $N }
    $total_calls = if ($lastParsed) { $lastParsed.total_calls } else { 0 }
    $total_oks   = if ($lastParsed) { $lastParsed.total_oks } else { 0 }
    $total_errs  = if ($lastParsed) { $lastParsed.total_errs } else { 0 }
    $err_rate    = if ($lastParsed -and $lastParsed.total_calls -gt 0) { $total_errs / $lastParsed.total_calls } else { 1.0 }
    if ($RepeatCount -gt 1) { Write-Host "  -> median lat_avg_ms=$([math]::Round($lat_avg_ms,1)) -> $(if ($pass) { 'PASS' } else { 'FAIL' })" -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' }) }
    $results.Add([PSCustomObject]@{
        backend = $CanonicalBackend
        tick_rate_hz = $CanonicalTickRateHz
        aps = $CanonicalAPS
        duration_s = $CanonicalDurationSec
        mode = $CanonicalMode
        server_physics = $CanonicalServerPhysics
        demo_entities = $CanonicalDemoEntities
        num_servers = 1
        players = $N
        total_calls = $total_calls
        total_oks = $total_oks
        total_errs = $total_errs
        err_rate_pct = [math]::Round($err_rate * 100, 2)
        lat_avg_ms = [math]::Round($lat_avg_ms, 2)
        pass = $pass
    })
    if ($FindCeiling -and -not $pass) {
        Write-Host "  Ceiling reached at N=$N (last pass N=$ceiling). Stopping."
        break
    }
    if ($CooldownSeconds -gt 0) { Start-Sleep -Seconds $CooldownSeconds }
}

$csvExists = Test-Path $OutCsv
$results | Export-Csv -Path $OutCsv -NoTypeInformation -Append:$csvExists
Write-Host "`n$(if ($csvExists) { 'Appended' } else { 'Wrote' }) : $OutCsv" -ForegroundColor Green
Write-Host "--- Summary ---" -ForegroundColor Cyan
if ($ceiling) { Write-Host "  Ceiling (server-physics): N=$ceiling" } else { Write-Host "  No passing run." }
Write-Host "  Passed: $(($results | Where-Object { $_.pass }).Count) / $($results.Count) runs"
Write-Host "Done."
