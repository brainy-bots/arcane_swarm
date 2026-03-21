<# 
.SYNOPSIS
  Full benchmark with incremental player-count steps (no swarm restart per step).

.DESCRIPTION
  Runs 30+ minutes. Run in a separate PowerShell window to avoid freezing your IDE/editor.

  This script expects the vendored swarm binary to support a control interface:
    --run-forever --control-port <port>
  Commands over TCP:
    SET_PLAYERS <n>
    RESET
    REPORT   (prints a line matching: FINAL: players=... total_calls=... total_oks=... total_errs=... lat_avg_ms=...)
    QUIT

  We still restart Arcane processes between different cluster-count experiments (num_servers), but we do not restart the swarm
  between 30s measurement windows; instead we adjust player count in-process.
#>

param(
    [switch] $NoPublish = $false,

    [int] $SpacetimeStep = 250,
    [int] $SpacetimeMaxPlayers = 2000,
    [int] $DurationSeconds = 30,

    [switch] $FindArcaneCeiling = $true,
    [int[]] $ArcaneClusterCounts = @(1, 2, 3, 4, 5, 10),
    [int] $ArcaneCeilingStartPlayers = 1500,
    [int] $ArcaneCeilingStep = 250,
    [int] $ArcaneCeilingMaxPlayers = 6000,

    [int] $PersistBatchSize = 0,

    [double] $MaxErrRate = 0.01,
    [double] $MaxLatencyMs = 200,

    [string] $SpacetimeHost = "http://127.0.0.1:3000",
    [string] $DatabaseName = "arcane",

    [string] $OutDir = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir = $PSScriptRoot
$BenchmarkRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

#
# Thin wrapper: delegate all benchmark logic to Run-Benchmark-Scenarios.ps1
# (no duplicated stepping logic).
#
$ScenarioScript = Join-Path $ScriptDir "Run-Benchmark-Scenarios.ps1"
$BaseOutDir = $OutDir
if ([string]::IsNullOrWhiteSpace($BaseOutDir)) {
    $BaseOutDir = Join-Path $ScriptDir "full_benchmark_incremental_runs"
}

# SpacetimeDB-only scenario (no Arcane cluster scenarios)
& $ScenarioScript `
    -SpacetimeHost $SpacetimeHost `
    -DatabaseName $DatabaseName `
    -StartPlayers $SpacetimeStep `
    -StepPlayers $SpacetimeStep `
    -MaxPlayers $SpacetimeMaxPlayers `
    -IncrementWindowSeconds $DurationSeconds `
    -BetweenIncrementsSeconds 1 `
    -TickRateHz 10 `
    -ActionsPerSec 2 `
    -ReadRateHz 5 `
    -SwarmMode "spread" `
    -MaxErrRate $MaxErrRate `
    -MaxLatencyMs $MaxLatencyMs `
    -ArcaneClusterCounts @() `
    -PersistBatchSize $PersistBatchSize `
    -OutDir (Join-Path $BaseOutDir "spacetimedb_only")

if ($FindArcaneCeiling) {
    # Arcane+Spacetime scenarios for each num_servers
    & $ScenarioScript `
        -SpacetimeHost $SpacetimeHost `
        -DatabaseName $DatabaseName `
        -StartPlayers $ArcaneCeilingStartPlayers `
        -StepPlayers $ArcaneCeilingStep `
        -MaxPlayers $ArcaneCeilingMaxPlayers `
        -IncrementWindowSeconds $DurationSeconds `
        -BetweenIncrementsSeconds 1 `
        -TickRateHz 10 `
        -ActionsPerSec 2 `
        -ReadRateHz 5 `
        -SwarmMode "spread" `
        -MaxErrRate $MaxErrRate `
        -MaxLatencyMs $MaxLatencyMs `
        -ArcaneClusterCounts $ArcaneClusterCounts `
        -PersistBatchSize $PersistBatchSize `
        -OutDir (Join-Path $BaseOutDir "arcane_plus_spacetimedb")
}

return

function Stop-ArcaneProcesses {
    $procs = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^arcane-' }
    if ($procs) {
        Write-Host "Stopping $($procs.Count) existing arcane-* processes..." -ForegroundColor Yellow
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}

function Send-SwarmCommand([int]$Port, [string]$CommandLine) {
    $client = New-Object System.Net.Sockets.TcpClient
    $client.Connect("127.0.0.1", $Port)
    $stream = $client.GetStream()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes(($CommandLine.TrimEnd() + "`n"))
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
    $client.Close()
}

function Parse-FinalLine([string]$Content) {
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

function Test-Pass([object]$Parsed, [double]$MaxErrRate, [double]$MaxLatencyMs) {
    if (-not $Parsed) { return $false }
    $errRate = if ($Parsed.total_calls -gt 0) { $Parsed.total_errs / $Parsed.total_calls } else { 1.0 }
    return ($errRate -lt $MaxErrRate -and $Parsed.lat_avg_ms -lt $MaxLatencyMs)
}

function Ensure-Build {
    param(
        [Parameter(Mandatory=$true)][string] $Dir,
        [Parameter(Mandatory=$true)][string] $CargoArgs
    )
    Push-Location $Dir
    try {
        cmd /c $CargoArgs
    } finally {
        Pop-Location
    }
}

if ($OutDir -eq "") {
    $OutDir = Join-Path $ScriptDir "full_benchmark_incremental"
}
$null = New-Item -ItemType Directory -Path $OutDir -Force

$spOutCsv = Join-Path $OutDir "spacetimedb_ceiling_sweep_incremental.csv"
$arcOutCsv = Join-Path $OutDir "arcane_scaling_sweep_incremental.csv"
$arcLogDir = Join-Path $OutDir "arcane_scaling_logs_incremental"
$null = New-Item -ItemType Directory -Path $arcLogDir -Force

$swarmCrateRoot = Join-Path $BenchmarkRoot "crates\arcane-benchmark-swarm"
$swarmExe = Join-Path $swarmCrateRoot "target\release\arcane-swarm.exe"
$arcaneRepo = Join-Path $BenchmarkRoot "arcane"
$arcaneExeManager = Join-Path $arcaneRepo "target\release\arcane-manager.exe"
$arcaneExeCluster = Join-Path $arcaneRepo "target\release\arcane-cluster.exe"
$modulePath = Join-Path $BenchmarkRoot "spacetimedb_demo\spacetimedb"

Stop-ArcaneProcesses

# Publish SpacetimeDB module once
if (-not $NoPublish) {
    Write-Host "Publishing SpacetimeDB module..." -ForegroundColor Cyan
    if (-not (Get-Command spacetime -ErrorAction SilentlyContinue)) { throw "SpacetimeDB CLI not found. Install or set PATH." }
    Push-Location $modulePath
    try {
        cmd /c "spacetime build 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime build failed" }
        cmd /c "spacetime publish $DatabaseName --yes 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime publish failed. Is 'spacetime start' running?" }
    } finally { Pop-Location }
}

# Ensure swarm binary built
if (-not (Test-Path $swarmExe)) {
    Write-Host "Building vendored arcane-swarm..." -ForegroundColor Yellow
    Ensure-Build -Dir $swarmCrateRoot -CargoArgs "cargo build --bin arcane-swarm --release"
}

# Ensure Arcane binaries built (manager + cluster)
if (-not (Test-Path $arcaneExeManager)) {
    Write-Host "Building arcane-manager..." -ForegroundColor Yellow
    Ensure-Build -Dir $arcaneRepo -CargoArgs "cargo build -p arcane-infra --bin arcane-manager --features manager --release"
}
if (-not (Test-Path $arcaneExeCluster)) {
    Write-Host "Building arcane-cluster..." -ForegroundColor Yellow
    Ensure-Build -Dir $arcaneRepo -CargoArgs "cargo build -p arcane-infra --bin arcane-cluster --features `"cluster-ws spacetimedb-persist`" --release"
}

function Run-SpacetimeDB-Incremental([int]$StartPlayers, [int]$Step, [int]$MaxPlayers, [int]$ControlPort, [string]$LogPrefix) {
    Stop-ArcaneProcesses

    $tmpOut = Join-Path $OutDir "${LogPrefix}_swarm_stdout.log"
    $tmpErr = Join-Path $OutDir "${LogPrefix}_swarm_stderr.log"
    if (Test-Path $tmpOut) { Remove-Item $tmpOut -Force }
    if (Test-Path $tmpErr) { Remove-Item $tmpErr -Force }

    Write-Host "Starting SpacetimeDB swarm control on port $ControlPort..." -ForegroundColor Cyan
    $proc = Start-Process -FilePath $swarmExe -WorkingDirectory $swarmCrateRoot -PassThru -NoNewWindow `
        -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr -ArgumentList @(
            "--backend", "spacetimedb",
            "--server-physics",
            "--players", $StartPlayers,
            "--max-players", $MaxPlayers,
            "--tick-rate", 10,
            "--aps", 2,
            "--duration", 0,
            "--mode", "spread",
            "--read-rate", 5,
            "--run-forever",
            "--control-port", $ControlPort,
            "--uri", $SpacetimeHost,
            "--db", $DatabaseName
        )

    $ceiling = $null
    $players = $StartPlayers
    $lastPass = $null
    $done = $false

    while (-not $done -and $players -le $MaxPlayers) {
        Write-Host "  [SpacetimeDB] testing players=$players ..." -ForegroundColor Gray

        Send-SwarmCommand -Port $ControlPort -CommandLine "SET_PLAYERS $players"
        Start-Sleep -Seconds 3

        # Reset metrics for this window
        Send-SwarmCommand -Port $ControlPort -CommandLine "RESET"

        Start-Sleep -Seconds $DurationSeconds

        Send-SwarmCommand -Port $ControlPort -CommandLine "REPORT"
        Start-Sleep -Seconds 1

        $content = ""
        if (Test-Path $tmpErr) { $content += (Get-Content $tmpErr -Raw -ErrorAction SilentlyContinue) }
        $parsed = Parse-FinalLine $content
        $pass = Test-Pass $parsed $MaxErrRate $MaxLatencyMs

        if ($pass) {
            $lastPass = $players
            $ceiling = $players
            $players += $Step
        } else {
            $done = $true
        }
    }

    Send-SwarmCommand -Port $ControlPort -CommandLine "QUIT"
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue

    return $ceiling
}

if (-not $NoPublish) {
    # no-op; publish above
}

Write-Host "`n=== SpacetimeDB-only incremental ceiling ===" -ForegroundColor Green
$spCeiling = Run-SpacetimeDB-Incremental -StartPlayers $SpacetimeStep -Step $SpacetimeStep -MaxPlayers $SpacetimeMaxPlayers -ControlPort 9100 -LogPrefix "spacetimedb_only"
Write-Host "SpacetimeDB-only incremental ceiling: $spCeiling players" -ForegroundColor Green

if (-not $FindArcaneCeiling) {
    Stop-ArcaneProcesses
    Write-Host "Arcane ceiling search disabled by flag. Done." -ForegroundColor Yellow
    return
}

foreach ($n in $ArcaneClusterCounts) {
    Write-Host "`n=== Arcane+Spacetime incremental ceiling (num_servers=$n) ===" -ForegroundColor Green

    Stop-ArcaneProcesses

    # Start manager + clusters
    $clusterBasePort = 8090
    $managerPort = 8081

    $managerLog = Join-Path $arcLogDir "manager_${n}.log"
    $managerErr = Join-Path $arcLogDir "manager_${n}_err.log"
    $clusterLogPrefix = Join-Path $arcLogDir "cluster_${n}"

    $clusterIds = @()
    $clusterPids = @()

    for ($i=0; $i -lt $n; $i++) {
        $clusterIds += ([guid]::NewGuid().ToString())
    }

    $managerClusters = @()
    for ($i=0; $i -lt $n; $i++) {
        $port = $clusterBasePort + $i
        $managerClusters += "${($clusterIds[$i])}:127.0.0.1:${port}"
    }
    $env:MANAGER_CLUSTERS = ($managerClusters -join ",")
    $env:MANAGER_HTTP_PORT = $managerPort

    $procManager = Start-Process -FilePath $arcaneExeManager -WorkingDirectory $arcaneRepo -NoNewWindow -PassThru `
        -RedirectStandardOutput $managerLog -RedirectStandardError $managerErr
    Start-Sleep -Seconds 2

    # Cluster processes
    $env:REDIS_URL = "redis://127.0.0.1:6379"
    $env:SPACETIMEDB_PERSIST = "1"
    $env:SPACETIMEDB_URI = $SpacetimeHost
    $env:SPACETIMEDB_DATABASE = $DatabaseName
    $env:SPACETIMEDB_PERSIST_HZ = "1"
    $env:SPACETIMEDB_PERSIST_BATCH_SIZE = $PersistBatchSize.ToString()
    $env:DEMO_ENTITIES = "0"

    for ($i=0; $i -lt $n; $i++) {
        $env:CLUSTER_ID = $clusterIds[$i]
        $env:CLUSTER_WS_PORT = ($clusterBasePort + $i).ToString()
        $neighborList = $clusterIds | Where-Object { $_ -ne $clusterIds[$i] }
        $env:NEIGHBOR_IDS = ($neighborList -join ",")

        $clog = "${clusterLogPrefix}_${i}.log"
        $cerr = "${clusterLogPrefix}_${i}_err.log"
        $p = Start-Process -FilePath $arcaneExeCluster -WorkingDirectory $arcaneRepo -NoNewWindow -PassThru `
            -RedirectStandardOutput $clog -RedirectStandardError $cerr
        $clusterPids += $p.Id
    }

    Start-Sleep -Seconds 3

    # Start swarm control
    $controlPort = 9200 + $n
    $tmpOut = Join-Path $OutDir "arcane_${n}_swarm_stdout.log"
    $tmpErr = Join-Path $OutDir "arcane_${n}_swarm_stderr.log"
    if (Test-Path $tmpOut) { Remove-Item $tmpOut -Force }
    if (Test-Path $tmpErr) { Remove-Item $tmpErr -Force }

    $startPlayers = $ArcaneCeilingStartPlayers
    $maxPlayers = $ArcaneCeilingMaxPlayers
    $procSwarm = Start-Process -FilePath $swarmExe -WorkingDirectory $swarmCrateRoot -PassThru -NoNewWindow `
        -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr -ArgumentList @(
            "--backend", "arcane",
            "--players", $startPlayers,
            "--max-players", $maxPlayers,
            "--tick-rate", 10,
            "--aps", 2,
            "--duration", 0,
            "--mode", "spread",
            "--read-rate", 5,
            "--run-forever",
            "--control-port", $controlPort,
            "--arcane-manager", "$SpacetimeHost" # placeholder, overridden below
        )

    # Use manager join: manager URL is always http://127.0.0.1:8081 (fixed in this script)
    # so we restart swarm with correct arg to avoid editing running process args.
    Send-SwarmCommand -Port $controlPort -CommandLine "QUIT"
    Stop-Process -Id $procSwarm.Id -Force -ErrorAction SilentlyContinue

    $procSwarm = Start-Process -FilePath $swarmExe -WorkingDirectory $swarmCrateRoot -PassThru -NoNewWindow `
        -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr -ArgumentList @(
            "--backend", "arcane",
            "--players", $startPlayers,
            "--max-players", $maxPlayers,
            "--tick-rate", 10,
            "--aps", 2,
            "--duration", 0,
            "--mode", "spread",
            "--read-rate", 5,
            "--run-forever",
            "--control-port", $controlPort,
            "--arcane-manager", "http://127.0.0.1:$managerPort"
        )

    $players = $startPlayers
    $ceiling = $null
    $lastPass = $null
    while ($players -le $maxPlayers) {
        Write-Host "  [Arcane n=$n] testing players=$players ..." -ForegroundColor Gray
        Send-SwarmCommand -Port $controlPort -CommandLine "SET_PLAYERS $players"
        Start-Sleep -Seconds 3
        Send-SwarmCommand -Port $controlPort -CommandLine "RESET"
        Start-Sleep -Seconds $DurationSeconds
        Send-SwarmCommand -Port $controlPort -CommandLine "REPORT"
        Start-Sleep -Seconds 1

        $content = ""
        if (Test-Path $tmpErr) { $content += (Get-Content $tmpErr -Raw -ErrorAction SilentlyContinue) }
        $parsed = Parse-FinalLine $content
        $pass = Test-Pass $parsed $MaxErrRate $MaxLatencyMs

        if ($pass) {
            $ceiling = $players
            $lastPass = $players
            $players += $ArcaneCeilingStep
        } else {
            break
        }
    }

    Send-SwarmCommand -Port $controlPort -CommandLine "QUIT"
    Stop-Process -Id $procSwarm.Id -Force -ErrorAction SilentlyContinue

    # Stop cluster processes
    foreach ($pid in $clusterPids) {
        Stop-Process -Id $pid -Force -ErrorAction SilentlyContinue
    }
    Stop-Process -Id $procManager.Id -Force -ErrorAction SilentlyContinue

    Write-Host "Arcane+Spacetime incremental ceiling for num_servers=${n}: $ceiling players" -ForegroundColor Green
}

Stop-ArcaneProcesses
Write-Host "`n=== DONE (Incremental) ===" -ForegroundColor Cyan

