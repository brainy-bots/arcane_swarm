<# 
.SYNOPSIS
  Run one benchmark scenario per cluster count, increasing players until failure.

.DESCRIPTION
  This script:
    - Ensures SpacetimeDB is running (starts it if needed).
    - Publishes the vendored SpacetimeDB module once.
    - Runs:
        1) SpacetimeDB-only (server_physics mode)
        2) Arcane+SpacetimeDB for N clusters (N=1..etc)
    - For each scenario:
        - starts the required Arcane processes once
        - starts the swarm in control mode (run indefinitely)
        - increases player count in steps, using RESET/REPORT for each window
        - stops when the acceptance criteria fails
        - cleans up Arcane processes afterward
    
  Acceptance is based on the swarm's FINAL line parsed from stderr:
    err_rate = total_errs / total_calls
    pass if err_rate < MaxErrRate AND lat_avg_ms < MaxLatencyMs
#>

param(
    # SpacetimeDB
    [string] $SpacetimeHost = "http://127.0.0.1:3000",
    [string] $DatabaseName = "arcane",

    # Redis (required for Arcane replication)
    [string] $RedisHost = "127.0.0.1",
    [int] $RedisPort = 6379,

    # Player stepping
    [int] $StartPlayers = 1000,
    [int] $StepPlayers = 250,
    [int] $MaxPlayers = 6000,
    # Measurement window per increment (after RESET, before REPORT)
    [int] $IncrementWindowSeconds = 30,
    # Extra sleep after REPORT before applying the next SET_PLAYERS
    [int] $BetweenIncrementsSeconds = 1,

    # Canonical workload parameters (exposed for reproducibility / iteration)
    [int] $TickRateHz = 10,
    [double] $ActionsPerSec = 2,
    [double] $ReadRateHz = 5,
    [string] $SwarmMode = "spread",

    [double] $MaxErrRate = 0.01,
    [double] $MaxLatencyMs = 200,

    # Arcane scenarios
    [int[]] $ArcaneClusterCounts = @(1,2,3,4,5,10),
    [int] $PersistBatchSize = 0,
    [int] $SpacetimePersistHz = 1,

    # Output
    [string] $OutDir = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir = $PSScriptRoot
$BenchmarkRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

$SwarmCrateRoot = Join-Path $BenchmarkRoot "crates\arcane-benchmark-swarm"
$SwarmExe = Join-Path $SwarmCrateRoot "target\release\arcane-swarm.exe"

$ArcaneRepo = Join-Path $BenchmarkRoot "arcane"
$ArcaneManagerExe = Join-Path $ArcaneRepo "target\release\arcane-manager.exe"
$ArcaneClusterExe = Join-Path $ArcaneRepo "target\release\arcane-cluster.exe"

$ModulePath = Join-Path $BenchmarkRoot "spacetimedb_demo\spacetimedb"

if ($OutDir -eq "") {
    $OutDir = Join-Path $ScriptDir "benchmark_scenarios_runs"
}
$null = New-Item -ItemType Directory -Path $OutDir -Force

$StdErrDir = Join-Path $OutDir "stderr"
$null = New-Item -ItemType Directory -Path $StdErrDir -Force

function Stop-ArcaneProcesses {
    $procs = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^arcane-' }
    if ($procs) {
        Write-Host "Stopping $($procs.Count) arcane-* processes..." -ForegroundColor Yellow
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}

function Wait-TcpOpen([string] $Host, [int] $Port, [int] $TimeoutSeconds) {
    for ($i=0; $i -lt $TimeoutSeconds; $i++) {
        $ok = (Test-NetConnection -ComputerName $Host -Port $Port -WarningAction SilentlyContinue).TcpTestSucceeded
        if ($ok) { return $true }
        Start-Sleep -Seconds 1
    }
    return $false
}

function Assert-ProcessAlive([int[]] $Pids, [string] $What) {
    foreach ($pid in $Pids) {
        if (-not $pid) { continue }
        $p = Get-Process -Id $pid -ErrorAction SilentlyContinue
        if (-not $p) {
            throw "$What: process id $pid is not running"
        }
    }
}

function Safe-Kill([int] $Pid, [string] $What) {
    if (-not $Pid) { return }
    try {
        $p = Get-Process -Id $Pid -ErrorAction SilentlyContinue
        if ($p) {
            Stop-Process -Id $Pid -Force -ErrorAction SilentlyContinue
            Start-Sleep -Milliseconds 500
        }
    } catch {
        Write-Warning "Failed to stop $What (pid=$Pid): $($_.Exception.Message)"
    }
}

function Ensure-SpacetimeRunning {
    # We only know SpacetimeDB is reachable by port; module publish requires it.
    $dbUrl = $SpacetimeHost
    $host = "127.0.0.1"
    $port = 3000
    if ($dbUrl -match "http://([^:/]+):?(\d+)?") {
        $host = $Matches[1]
        if ($Matches[2]) { $port = [int]$Matches[2] }
    }

    if ((Test-NetConnection -ComputerName $host -Port $port -WarningAction SilentlyContinue).TcpTestSucceeded) {
        return
    }

    if (-not (Get-Command spacetime -ErrorAction SilentlyContinue)) {
        throw "SpacetimeDB not reachable on $host:$port and 'spacetime' CLI not found. Install or set PATH."
    }

    Write-Host "SpacetimeDB not reachable on $host:$port; starting 'spacetime start'..." -ForegroundColor Cyan

    $logPath = Join-Path $OutDir "spacetime_start.log"
    if (Test-Path $logPath) { Remove-Item $logPath -Force }

    $proc = Start-Process -FilePath "spacetime" -ArgumentList @("start") -NoNewWindow -PassThru `
        -RedirectStandardOutput $logPath -RedirectStandardError $logPath

    # Poll until reachable
    for ($i=0; $i -lt 120; $i++) {
        if ((Test-NetConnection -ComputerName $host -Port $port -WarningAction SilentlyContinue).TcpTestSucceeded) {
            Write-Host "SpacetimeDB is reachable." -ForegroundColor Green
            return
        }
        Start-Sleep -Seconds 1
    }
    throw "Timed out waiting for SpacetimeDB to start. Log: $logPath"
}

function Publish-Module {
    Write-Host "Publishing SpacetimeDB module..." -ForegroundColor Cyan
    Push-Location $ModulePath
    try {
        cmd /c "spacetime build 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime build failed" }
        cmd /c "spacetime publish $DatabaseName --yes 2>&1"
        if ($LASTEXITCODE -ne 0) { throw "spacetime publish failed. Is 'spacetime start' running?" }
    } finally { Pop-Location }
}

function Ensure-Binary {
    param([Parameter(Mandatory=$true)][string] $Path, [Parameter(Mandatory=$true)][string] $BuildCommand, [Parameter(Mandatory=$true)][string] $WorkDir)
    if (-not (Test-Path $Path)) {
        Write-Host "Building: $Path" -ForegroundColor Yellow
        Push-Location $WorkDir
        cmd /c $BuildCommand
        if ($LASTEXITCODE -ne 0) { Pop-Location; throw "Build failed for: $Path" }
        Pop-Location
    }
}

function Send-SwarmCommand([int]$Port, [string]$Line) {
    $client = New-Object System.Net.Sockets.TcpClient
    $client.Connect("127.0.0.1", $Port)
    $stream = $client.GetStream()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes(($Line.TrimEnd() + "`n"))
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
    $client.Close()
}

function Parse-SwarmFinal([string] $Text) {
    $re = "FINAL:\s*players=(\d+)\s+total_calls=(\d+)\s+total_oks=(\d+)\s+total_errs=(\d+)\s+lat_avg_ms=([\d.]+)"
    $all = [regex]::Matches($Text, $re)
    if ($all.Count -eq 0) { return $null }

    $m = $all[$all.Count - 1]
    return [PSCustomObject]@{
        players = [int]$m.Groups[1].Value
        total_calls = [long]$m.Groups[2].Value
        total_oks = [long]$m.Groups[3].Value
        total_errs = [long]$m.Groups[4].Value
        lat_avg_ms = [double]$m.Groups[5].Value
    }
}

function Is-Pass([object] $Parsed) {
    if (-not $Parsed) { return $false }
    $errRate = if ($Parsed.total_calls -gt 0) { $Parsed.total_errs / $Parsed.total_calls } else { 1.0 }
    return (($errRate -lt $MaxErrRate) -and ($Parsed.lat_avg_ms -lt $MaxLatencyMs))
}

function Run-Scenario-SpacetimeOnly {
    param(
        [int] $ControlPort,
        [int] $ScenarioStartPlayers,
        [int] $ScenarioStepPlayers,
        [int] $ScenarioMaxPlayers
    )

    $stderr = Join-Path $StdErrDir "spacetimedb_only_${ControlPort}_stderr.log"
    if (Test-Path $stderr) { Remove-Item $stderr -Force }

    Write-Host "SpacetimeDB-only scenario control port $ControlPort" -ForegroundColor Cyan

    $proc = Start-Process -FilePath $SwarmExe -WorkingDirectory $SwarmCrateRoot -NoNewWindow -PassThru `
        -RedirectStandardOutput (Join-Path $StdErrDir "spacetimedb_only_${ControlPort}_stdout.log") `
        -RedirectStandardError $stderr `
        -ArgumentList @(
            "--backend","spacetimedb",
            "--server-physics",
            "--players",$ScenarioStartPlayers,
            "--max-players",$ScenarioMaxPlayers,
            "--tick-rate",$TickRateHz,
            "--aps",$ActionsPerSec,
            "--mode",$SwarmMode,
            "--read-rate",$ReadRateHz,
            "--duration","0",
            "--run-forever",
            "--control-port",$ControlPort,
            "--uri",$SpacetimeHost,
            "--db",$DatabaseName
        )

    if (-not (Wait-TcpOpen -Host "127.0.0.1" -Port $ControlPort -TimeoutSeconds 20)) {
        throw "swarm control port $ControlPort was not opened (spacetime-only scenario)"
    }

    $players = $ScenarioStartPlayers
    $ceiling = $null
    try {
        while ($players -le $ScenarioMaxPlayers) {
            Write-Host "  [SpacetimeDB-only] testing players=$players ..." -ForegroundColor Gray
            Send-SwarmCommand -Port $ControlPort -Line "SET_PLAYERS $players"
            Start-Sleep -Seconds 2
            Send-SwarmCommand -Port $ControlPort -Line "RESET"
            Start-Sleep -Seconds $IncrementWindowSeconds
            Send-SwarmCommand -Port $ControlPort -Line "REPORT"

            Start-Sleep -Seconds $BetweenIncrementsSeconds
            $txt = ""
            if (Test-Path $stderr) { $txt = Get-Content -Path $stderr -Raw -ErrorAction SilentlyContinue }
            $parsed = Parse-SwarmFinal $txt
            $pass = Is-Pass $parsed

            if ($pass) {
                $ceiling = $players
                $players += $ScenarioStepPlayers
            } else {
                break
            }
        }
    } finally {
        Send-SwarmCommand -Port $ControlPort -Line "QUIT"
        Safe-Kill -Pid $proc.Id -What "swarm"
    }

    return $ceiling
}

function Run-Scenario-Arcane {
    param(
        [int] $NumServers,
        [int] $ControlPort,
        [int] $ScenarioStartPlayers,
        [int] $ScenarioStepPlayers,
        [int] $ScenarioMaxPlayers
    )

    Stop-ArcaneProcesses

    $clusterBasePort = 8090
    $managerPort = 8081

    # Clean ports between runs
    Stop-ArcaneProcesses

    $clusterIds = for ($i=0; $i -lt $NumServers; $i++) { [guid]::NewGuid().ToString() }
    $clusterPids = @()

    # Manager env
    $managerClusters = @()
    for ($i=0; $i -lt $NumServers; $i++) {
        $port = $clusterBasePort + $i
        $managerClusters += "${($clusterIds[$i])}:127.0.0.1:${port}"
    }
    $env:MANAGER_CLUSTERS = ($managerClusters -join ",")
    $env:MANAGER_HTTP_PORT = $managerPort

    $managerLog = Join-Path $StdErrDir "manager_${NumServers}_stdout.log"
    $managerErr = Join-Path $StdErrDir "manager_${NumServers}_stderr.log"
    if (Test-Path $managerLog) { Remove-Item $managerLog -Force }
    if (Test-Path $managerErr) { Remove-Item $managerErr -Force }

    Write-Host "Arcane scenario num_servers=$NumServers" -ForegroundColor Cyan
    $procManager = Start-Process -FilePath $ArcaneManagerExe -WorkingDirectory $ArcaneRepo -NoNewWindow -PassThru `
        -RedirectStandardOutput $managerLog -RedirectStandardError $managerErr
    Start-Sleep -Seconds 2

    # Clusters env
    $env:REDIS_URL = "redis://$RedisHost:$RedisPort"
    $env:SPACETIMEDB_PERSIST = "1"
    $env:SPACETIMEDB_URI = $SpacetimeHost
    $env:SPACETIMEDB_DATABASE = $DatabaseName
    $env:SPACETIMEDB_PERSIST_HZ = $SpacetimePersistHz.ToString()
    $env:SPACETIMEDB_PERSIST_BATCH_SIZE = $PersistBatchSize.ToString()

    for ($i=0; $i -lt $NumServers; $i++) {
        $env:CLUSTER_ID = $clusterIds[$i]
        $env:CLUSTER_WS_PORT = ($clusterBasePort + $i).ToString()
        $neighborList = $clusterIds | Where-Object { $_ -ne $clusterIds[$i] }
        $env:NEIGHBOR_IDS = ($neighborList -join ",")

        $clog = Join-Path $StdErrDir "cluster_${NumServers}_${i}_stdout.log"
        $cerr = Join-Path $StdErrDir "cluster_${NumServers}_${i}_stderr.log"
        if (Test-Path $clog) { Remove-Item $clog -Force }
        if (Test-Path $cerr) { Remove-Item $cerr -Force }

        $p = Start-Process -FilePath $ArcaneClusterExe -WorkingDirectory $ArcaneRepo -NoNewWindow -PassThru `
            -RedirectStandardOutput $clog -RedirectStandardError $cerr
        $clusterPids += $p.Id
    }

    Start-Sleep -Seconds 3

    # Verify manager and clusters are actually accepting connections
    if (-not (Wait-TcpOpen -Host "127.0.0.1" -Port $managerPort -TimeoutSeconds 20)) {
        throw "arcane-manager did not open port $managerPort"
    }
    for ($i=0; $i -lt $NumServers; $i++) {
        $wsPort = $clusterBasePort + $i
        if (-not (Wait-TcpOpen -Host "127.0.0.1" -Port $wsPort -TimeoutSeconds 20)) {
            throw "arcane-cluster[$i] did not open websocket port $wsPort"
        }
    }
    Assert-ProcessAlive -Pids $clusterPids -What "cluster"
    Assert-ProcessAlive -Pids @($procManager.Id) -What "manager"

    # Verify swarm control port is reachable (TCP control server in swarm)
    if (-not (Wait-TcpOpen -Host "127.0.0.1" -Port $ControlPort -TimeoutSeconds 20)) {
        throw "swarm control port $ControlPort was not opened"
    }

    $stderr = Join-Path $StdErrDir "arcane_${NumServers}_${ControlPort}_stderr.log"
    $stdout = Join-Path $StdErrDir "arcane_${NumServers}_${ControlPort}_stdout.log"
    if (Test-Path $stderr) { Remove-Item $stderr -Force }
    if (Test-Path $stdout) { Remove-Item $stdout -Force }

    $procSwarm = Start-Process -FilePath $SwarmExe -WorkingDirectory $SwarmCrateRoot -NoNewWindow -PassThru `
        -RedirectStandardOutput $stdout `
        -RedirectStandardError $stderr `
        -ArgumentList @(
            "--backend","arcane",
            "--players",$ScenarioStartPlayers,
            "--max-players",$ScenarioMaxPlayers,
            "--tick-rate",$TickRateHz,
            "--aps",$ActionsPerSec,
            "--mode",$SwarmMode,
            "--read-rate",$ReadRateHz,
            "--duration","0",
            "--run-forever",
            "--control-port",$ControlPort,
            "--arcane-manager","http://127.0.0.1:$managerPort",
            "--uri",$SpacetimeHost,
            "--db",$DatabaseName
        )

    $players = $ScenarioStartPlayers
    $ceiling = $null
    try {
        while ($players -le $ScenarioMaxPlayers) {
            Write-Host "  [Arcane+Spacetime] num_servers=$NumServers testing players=$players ..." -ForegroundColor Gray
            Send-SwarmCommand -Port $ControlPort -Line "SET_PLAYERS $players"
            Start-Sleep -Seconds 2
            Send-SwarmCommand -Port $ControlPort -Line "RESET"
            Start-Sleep -Seconds $IncrementWindowSeconds
            Send-SwarmCommand -Port $ControlPort -Line "REPORT"

            Start-Sleep -Seconds $BetweenIncrementsSeconds
            $txt = ""
            if (Test-Path $stderr) { $txt = Get-Content -Path $stderr -Raw -ErrorAction SilentlyContinue }
            $parsed = Parse-SwarmFinal $txt
            $pass = Is-Pass $parsed

            if ($pass) {
                $ceiling = $players
                $players += $ScenarioStepPlayers
            } else {
                break
            }
        }
    } finally {
        Send-SwarmCommand -Port $ControlPort -Line "QUIT"
        Safe-Kill -Pid $procSwarm.Id -What "swarm"

        # Cleanup arcane processes
        foreach ($pid in $clusterPids) { Safe-Kill -Pid $pid -What "cluster" }
        Safe-Kill -Pid $procManager.Id -What "manager"
    }

    return $ceiling
}

# --- Bootstrap / prerequisites ---

# Redis must be up (Arcane replication needs it)
if (-not (Test-NetConnection -ComputerName $RedisHost -Port $RedisPort -WarningAction SilentlyContinue).TcpTestSucceeded) {
    throw "Redis not reachable at $RedisHost:$RedisPort. Start Redis before running."
}

Ensure-SpacetimeRunning
Publish-Module

Ensure-Binary -Path $SwarmExe -WorkDir $SwarmCrateRoot -BuildCommand "cargo build --bin arcane-swarm --release"
Ensure-Binary -Path $ArcaneManagerExe -WorkDir $ArcaneRepo -BuildCommand "cargo build -p arcane-infra --bin arcane-manager --features manager --release"
Ensure-Binary -Path $ArcaneClusterExe -WorkDir $ArcaneRepo -BuildCommand "cargo build -p arcane-infra --bin arcane-cluster --features `"cluster-ws spacetimedb-persist`" --release"

# --- Run scenarios ---

$results = @()

$spControlPort = 9300
$spCeiling = Run-Scenario-SpacetimeOnly -ControlPort $spControlPort -ScenarioStartPlayers $StartPlayers -ScenarioStepPlayers $StepPlayers -ScenarioMaxPlayers $MaxPlayers
$results += [PSCustomObject]@{ backend = "spacetimedb_only"; num_servers = 0; ceiling_players = $spCeiling }

foreach ($n in $ArcaneClusterCounts) {
    $controlPort = 9400 + $n
    $ceiling = Run-Scenario-Arcane -NumServers $n -ControlPort $controlPort -ScenarioStartPlayers $StartPlayers -ScenarioStepPlayers $StepPlayers -ScenarioMaxPlayers $MaxPlayers
    $results += [PSCustomObject]@{ backend = "arcane_plus_spacetimedb"; num_servers = $n; ceiling_players = $ceiling }
}

$csv = Join-Path $OutDir "benchmark_scenarios_results.csv"
$results | Export-Csv -Path $csv -NoTypeInformation
Write-Host "Results written to: $csv" -ForegroundColor Green

# Final cleanup so readers can re-run immediately
Stop-ArcaneProcesses

