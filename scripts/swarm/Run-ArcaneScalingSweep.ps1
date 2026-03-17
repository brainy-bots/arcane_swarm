<#
.SYNOPSIS
    Arcane + SpacetimeDB scaling test: physics on clusters, persistence to SpacetimeDB.

.DESCRIPTION
    Run from the arcane-scaling-benchmarks repo (clone with --recurse-submodules or run git submodule update --init).
    Starts N Arcane cluster servers (arcane-cluster-demo), arcane-manager, then arcane-swarm.
    Prerequisites: Redis (redis://127.0.0.1:6379), SpacetimeDB ("spacetime start" in another terminal).
    SpacetimeDB module is built/published from arcane-demos submodule unless -NoPublish.

.PARAMETER PersistBatchSize
    Default 0 (no cap) for best ceilings. Use 500 to reproduce cap=500 experiments.
#>
param(
    [int]   $NumServers = 1,
    [int]   $PlayersTotal = 250,
    [int]   $Duration = 30,
    [double] $MaxErrRate = 0.01,
    [double] $MaxLatencyMs = 200,
    [switch] $NoPublish,
    [string] $OutCsv = "",
    [string] $SpacetimeHost = "http://127.0.0.1:3000",
    [string] $DatabaseName = "arcane",
    [string] $LogDir = "",
    [int]    $PersistBatchSize = 0
)

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot
$BenchmarkRoot = Resolve-Path (Join-Path $ScriptDir "..\..")
$ArcaneDemosRepo = Join-Path $BenchmarkRoot "arcane-demos"
$ArcaneRepo = Join-Path $BenchmarkRoot "arcane"

if (-not (Test-Path $ArcaneDemosRepo)) {
    throw "arcane-demos submodule not found at $ArcaneDemosRepo. Run: git submodule update --init --recursive"
}
if (-not (Test-Path $ArcaneRepo)) {
    throw "arcane submodule not found at $ArcaneRepo. Run: git submodule update --init --recursive"
}

if ($OutCsv -eq "") { $OutCsv = Join-Path $ScriptDir "arcane_scaling_sweep.csv" }
if ($LogDir -eq "") { $LogDir = Join-Path $ScriptDir "arcane_scaling_logs" }
$null = New-Item -ItemType Directory -Path $LogDir -Force

# Canonical parameters (must match SpacetimeDB-only runs for comparable ceilings)
$CanonicalTickRateHz = 10
$CanonicalAPS = 2
$CanonicalDurationSec = 30
$CanonicalMode = "spread"
$CanonicalReadRateHz = 5
$CanonicalDemoEntities = 0
$CanonicalSpacetimePersistHz = 1
$CanonicalRedisEnabled = $true
$CanonicalBackend = "arcane_plus_spacetimedb"

$ClusterBasePort = 8090
$ManagerPort = 8081

# Paths (binaries from submodules)
$ExeSwarm = Join-Path $ArcaneDemosRepo "target\release\arcane-swarm.exe"
$ExeClusterDemo = Join-Path $ArcaneDemosRepo "target\release\arcane-cluster-demo.exe"
$ExeManager = Join-Path $ArcaneRepo "target\release\arcane-manager.exe"
$ModulePath = Join-Path $ArcaneDemosRepo "spacetimedb_demo\spacetimedb"

# Resolve SpacetimeDB CLI for publish
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
            $SpacetimeAvailable = $true
            break
        }
    }
}

# Build and publish SpacetimeDB module (unless -NoPublish)
if (-not $NoPublish) {
    if (-not $SpacetimeAvailable) { throw "SpacetimeDB CLI not found. Install or set PATH." }
    Write-Host "Building SpacetimeDB module..." -ForegroundColor Yellow
    Push-Location $ModulePath
    try {
        cmd /c "spacetime build 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime build failed" }
        cmd /c "spacetime publish $DatabaseName --yes 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime publish failed. Is 'spacetime start' running?" }
    } finally { Pop-Location }
    Write-Host "Module published to $SpacetimeHost / $DatabaseName" -ForegroundColor Green
}

# Build arcane-swarm and arcane-cluster-demo (arcane-demos submodule)
if (-not (Test-Path $ExeSwarm)) {
    Write-Host "Building arcane-swarm (release)..." -ForegroundColor Yellow
    Push-Location $ArcaneDemosRepo
    cmd /c "cargo build -p arcane-demo --bin arcane-swarm --features swarm --release 2>&1"
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "arcane-swarm build failed" }
    Pop-Location
}
if (-not (Test-Path $ExeClusterDemo)) {
    Write-Host "Building arcane-cluster-demo (release)..." -ForegroundColor Yellow
    Push-Location $ArcaneDemosRepo
    cmd /c "cargo build -p arcane-demo --bin arcane-cluster-demo --release 2>&1"
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "arcane-cluster-demo build failed" }
    Pop-Location
}

# Build arcane-manager (arcane submodule)
if (-not (Test-Path $ExeManager)) {
    Write-Host "Building arcane-manager (release)..." -ForegroundColor Yellow
    Push-Location $ArcaneRepo
    cmd /c "cargo build -p arcane-infra --bin arcane-manager --features manager --release 2>&1"
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "arcane-manager build failed" }
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

function Show-BottleneckSummary {
    param([string] $LogDirPath, [int] $NumClusters)
    Write-Host "`n--- Bottleneck visibility (logs in $LogDirPath) ---" -ForegroundColor Cyan
    for ($i = 0; $i -lt $NumClusters; $i++) {
        $logPath = Join-Path $LogDirPath "cluster_$i.log"
        $errPath = Join-Path $LogDirPath "cluster_${i}_err.log"
        $content = @()
        if (Test-Path $logPath) { $content += Get-Content $logPath -Raw -ErrorAction SilentlyContinue }
        if (Test-Path $errPath) { $content += Get-Content $errPath -Raw -ErrorAction SilentlyContinue }
        $text = $content -join "`n"
        $stats = [regex]::Matches($text, "ArcaneServerStats:\s*entities=(\d+)\s+clusters=(\d+)\s+tick_ms=([\d.]+)")
        $persist = [regex]::Matches($text, "SpacetimeDB persist: (\d+) entities in ([\d.]+)ms")
        $persistErr = [regex]::Matches($text, "SpacetimeDB persist error[^\n]*")
        if ($stats.Count -gt 0) {
            $last = $stats[$stats.Count - 1]
            $maxTickMs = ($stats | ForEach-Object { [double]$_.Groups[3].Value } | Measure-Object -Maximum).Maximum
            Write-Host "  Cluster $i : entities=$($last.Groups[1].Value) tick_ms(last)=$($last.Groups[3].Value) tick_ms(max)=$([math]::Round($maxTickMs,2))" -ForegroundColor Gray
        }
        if ($persist.Count -gt 0) {
            $lastP = $persist[$persist.Count - 1]
            Write-Host "  Cluster $i SpacetimeDB persist: $($lastP.Groups[1].Value) entities in $($lastP.Groups[2].Value)ms" -ForegroundColor Gray
        }
        if ($persistErr.Count -gt 0) {
            Write-Host "  Cluster $i SpacetimeDB persist ERRORS: $($persistErr.Count)" -ForegroundColor Red
        }
    }
    Write-Host "  Client-side: total_calls, errs, lat_avg_ms above = swarm (client->cluster). High tick_ms = cluster CPU; high persist ms or errors = SpacetimeDB." -ForegroundColor Gray
}

# Ensure no leftover Arcane processes (prevents port conflicts when re-running or after a crash)
$arcaneProcesses = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^arcane-' }
if ($arcaneProcesses) {
    Write-Host "Stopping $($arcaneProcesses.Count) existing Arcane process(es) to avoid port conflicts..." -ForegroundColor Yellow
    $arcaneProcesses | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

# Build MANAGER_CLUSTERS: id1:127.0.0.1:8090,id2:127.0.0.1:8091,...
$clusterEntries = @()
$clusterIds = @()
for ($i = 0; $i -lt $NumServers; $i++) {
    $cid = [guid]::NewGuid().ToString()
    $clusterIds += $cid
    $port = $ClusterBasePort + $i
    $clusterEntries += "${cid}:127.0.0.1:${port}"
}
$managerClusters = $clusterEntries -join ","

# Start manager (capture logs for visibility)
$env:MANAGER_CLUSTERS = $managerClusters
$env:MANAGER_HTTP_PORT = $ManagerPort
$managerLog = Join-Path $LogDir "manager.log"
Write-Host "Starting arcane-manager (port $ManagerPort) with $NumServers cluster(s); log: $managerLog" -ForegroundColor Cyan
$procManager = Start-Process -FilePath $ExeManager -WorkingDirectory $ArcaneRepo -PassThru -NoNewWindow `
    -RedirectStandardOutput $managerLog -RedirectStandardError (Join-Path $LogDir "manager_err.log")
$managerPid = $procManager.Id
Start-Sleep -Seconds 2

# Start cluster processes (physics + SpacetimeDB persist; Redis always on)
$clusterPids = @()
$env:REDIS_URL = "redis://127.0.0.1:6379"
$env:SPACETIMEDB_PERSIST = "1"
$env:SPACETIMEDB_URI = $SpacetimeHost
$env:SPACETIMEDB_DATABASE = $DatabaseName
$env:SPACETIMEDB_PERSIST_HZ = $CanonicalSpacetimePersistHz.ToString()
$env:SPACETIMEDB_PERSIST_BATCH_SIZE = $PersistBatchSize.ToString()
$env:DEMO_ENTITIES = $CanonicalDemoEntities.ToString()

for ($i = 0; $i -lt $NumServers; $i++) {
    $env:CLUSTER_ID = $clusterIds[$i]
    $env:CLUSTER_WS_PORT = ($ClusterBasePort + $i).ToString()
    $neighborList = $clusterIds | Where-Object { $_ -ne $clusterIds[$i] }
    $env:NEIGHBOR_IDS = ($neighborList -join ",")
    $clusterLog = Join-Path $LogDir "cluster_$i.log"
    $clusterErrLog = Join-Path $LogDir "cluster_${i}_err.log"
    Write-Host "  Starting cluster $i on port $($env:CLUSTER_WS_PORT) (physics + persist); log: $clusterLog" -ForegroundColor Gray
    $proc = Start-Process -FilePath $ExeClusterDemo -WorkingDirectory $ArcaneDemosRepo -PassThru -NoNewWindow `
        -RedirectStandardOutput $clusterLog -RedirectStandardError $clusterErrLog
    $clusterPids += $proc.Id
}
Start-Sleep -Seconds 3

# Verify manager and all clusters are still running (catch bind failures, e.g. port in use)
$managerGone = $null -eq (Get-Process -Id $managerPid -ErrorAction SilentlyContinue)
if ($managerGone) {
    $errContent = Get-Content (Join-Path $LogDir "manager_err.log") -Raw -ErrorAction SilentlyContinue
    throw "arcane-manager exited before swarm (check manager_err.log). Often port $ManagerPort in use. Stderr: $errContent"
}
for ($i = 0; $i -lt $NumServers; $i++) {
    $clusterGone = $null -eq (Get-Process -Id $clusterPids[$i] -ErrorAction SilentlyContinue)
    if ($clusterGone) {
        $errPath = Join-Path $LogDir "cluster_${i}_err.log"
        $errContent = Get-Content $errPath -Raw -ErrorAction SilentlyContinue
        throw "Cluster $i exited before swarm (e.g. bind failed; check $errPath). Often port $($ClusterBasePort + $i) in use. Stderr: $errContent"
    }
}

try {
    Write-Host "`n--- Canonical run parameters (fixed for all experiments) ---" -ForegroundColor Yellow
    Write-Host "  tick_rate_hz=$CanonicalTickRateHz  aps=$CanonicalAPS  duration_s=$CanonicalDurationSec  mode=$CanonicalMode  read_rate_hz=$CanonicalReadRateHz  demo_entities=$CanonicalDemoEntities  spacetimedb_persist_hz=$CanonicalSpacetimePersistHz  redis_enabled=$CanonicalRedisEnabled  backend=$CanonicalBackend"
    Write-Host "  visibility=everyone_sees_everyone  num_servers=$NumServers  pass_criteria=err_rate<$MaxErrRate lat_avg_ms<$MaxLatencyMs"
    Write-Host "---" -ForegroundColor Yellow
    Write-Host "Running arcane-swarm: $PlayersTotal players, $CanonicalDurationSec s, manager http://127.0.0.1:$ManagerPort" -ForegroundColor Cyan
    $tmpOut = [System.IO.Path]::GetTempFileName()
    $tmpErr = [System.IO.Path]::GetTempFileName()
    $proc = Start-Process -FilePath $ExeSwarm -ArgumentList @(
        "--backend", "arcane",
        "--arcane-manager", "http://127.0.0.1:$ManagerPort",
        "--players", $PlayersTotal,
        "--tick-rate", $CanonicalTickRateHz,
        "--actions-per-sec", $CanonicalAPS,
        "--duration", $CanonicalDurationSec,
        "--mode", $CanonicalMode,
        "--read-rate", $CanonicalReadRateHz
    ) -WorkingDirectory $ArcaneDemosRepo -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr -Wait -NoNewWindow -PassThru
    $out = Get-Content -Path $tmpOut -Raw -ErrorAction SilentlyContinue
    $err = Get-Content -Path $tmpErr -Raw -ErrorAction SilentlyContinue
    $all = if ($out) { $out } else { "" }; if ($err) { $all += "`n" + $err }
    Remove-Item -Path $tmpOut, $tmpErr -Force -ErrorAction SilentlyContinue

    $parsed = Parse-FinalLine $all
    $total_calls = if ($parsed) { $parsed.total_calls } else { 0 }
    $total_errs = if ($parsed) { $parsed.total_errs } else { 0 }
    $lat_avg_ms = if ($parsed) { $parsed.lat_avg_ms } else { 0.0 }
    $err_rate = if ($parsed -and $parsed.total_calls -gt 0) { $parsed.total_errs / $parsed.total_calls } else { 1.0 }
    $pass = $parsed -and ($err_rate -lt $MaxErrRate) -and ($lat_avg_ms -lt $MaxLatencyMs)

    Write-Host "  total_calls=$total_calls errs=$total_errs err_rate=$([math]::Round($err_rate*100,2))% lat_avg_ms=$([math]::Round($lat_avg_ms,1)) -> $(if ($pass) { 'PASS' } else { 'FAIL' })" -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' })

    $row = [PSCustomObject]@{
        backend = $CanonicalBackend
        tick_rate_hz = $CanonicalTickRateHz
        aps = $CanonicalAPS
        duration_s = $CanonicalDurationSec
        mode = $CanonicalMode
        read_rate_hz = $CanonicalReadRateHz
        demo_entities = $CanonicalDemoEntities
        spacetimedb_persist_hz = $CanonicalSpacetimePersistHz
        redis_enabled = $CanonicalRedisEnabled
        num_servers = $NumServers
        players = $PlayersTotal
        total_calls = $total_calls
        total_errs = $total_errs
        err_rate_pct = [math]::Round($err_rate * 100, 2)
        lat_avg_ms = [math]::Round($lat_avg_ms, 2)
        pass = $pass
    }
    $csvExists = Test-Path $OutCsv
    $row | Export-Csv -Path $OutCsv -NoTypeInformation -Append:$csvExists
    Write-Host "`n$(if ($csvExists) { 'Appended' } else { 'Wrote' }) : $OutCsv" -ForegroundColor Green
    Show-BottleneckSummary -LogDirPath $LogDir -NumClusters $NumServers
}
finally {
    Write-Host "Stopping manager and $NumServers cluster(s)..." -ForegroundColor Gray
    foreach ($clusterPid in $clusterPids) {
        Stop-Process -Id $clusterPid -Force -ErrorAction SilentlyContinue
    }
    Stop-Process -Id $managerPid -Force -ErrorAction SilentlyContinue
}
Write-Host "Done."
