<# 
.SYNOPSIS
  Run the full reproducibility benchmark end-to-end (SpacetimeDB-only + Arcane+SpacetimeDB).

.DESCRIPTION
  This is a convenience wrapper around:
    - Run-SpacetimeDBCeilingSweep.ps1
    - Run-ArcaneScalingSweep.ps1

  It:
    - cleans Arcane processes before/between runs
    - publishes SpacetimeDB module once, then reuses it for subsequent sweeps (via -NoPublish)
    - optionally finds ceilings by stepping player counts until pass->fail

  Incremental "increase players without restarting clusters" requires extra removal/reset plumbing in the
  Arcane cluster/server. This script currently prioritizes correctness by restarting the cluster per sweep,
  while still avoiding repeated SpacetimeDB rebuild/publish.
#>

param(
    [switch] $FindSpacetimeDBCeiling = $true,
    [int] $SpacetimeStep = 250,
    [int] $SpacetimeMaxPlayers = 2000,

    [switch] $FindArcaneCeiling = $true,
    [int[]] $ArcaneClusterCounts = @(1, 2, 3, 4, 5, 10),
    [int] $ArcaneCeilingStartPlayers = 1500,
    [int] $ArcaneCeilingStep = 250,
    [int] $ArcaneCeilingMaxPlayers = 6000,
    [int] $PersistBatchSize = 0,

    # If set, never publish SpacetimeDB module (assumes it's already published)
    [switch] $NoPublish = $false,

    [string] $SpacetimeHost = "http://127.0.0.1:3000",
    [string] $DatabaseName = "arcane",

    # Output
    [string] $OutDir = "",
    [int] $DurationSeconds = 30,
    [double] $MaxErrRate = 0.01,
    [double] $MaxLatencyMs = 200
)

$ErrorActionPreference = "Stop"

$ScriptDir = $PSScriptRoot
$BenchmarkRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

function Stop-ArcaneProcesses {
    $procs = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match '^arcane-' }
    if ($procs) {
        Write-Host "Stopping $($procs.Count) existing arcane-* process(es)..." -ForegroundColor Yellow
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}

if ($OutDir -eq "") {
    $OutDir = Join-Path $ScriptDir "full_benchmark_runs"
}
$null = New-Item -ItemType Directory -Path $OutDir -Force

$SpacetimeOutCsv = Join-Path $OutDir "spacetimedb_ceiling_sweep.csv"
$ArcaneOutCsv    = Join-Path $OutDir "arcane_scaling_sweep.csv"
$ArcaneLogDir    = Join-Path $OutDir "arcane_scaling_logs"
$null = New-Item -ItemType Directory -Path $ArcaneLogDir -Force

Write-Host "`n=== FULL BENCHMARK ===" -ForegroundColor Cyan
Write-Host "OutDir: $OutDir" -ForegroundColor Gray

# Always clean once at the beginning to avoid port/state conflicts.
Stop-ArcaneProcesses

$published = $false

function Run-SpacetimeDBSweepOnce([switch] $DoFindCeiling) {
    Stop-ArcaneProcesses
    $extra = @(
        "-OutCsv", $SpacetimeOutCsv,
        "-CooldownSeconds", 0,
        "-RepeatCount", 1,
        "-SpacetimeHost", $SpacetimeHost,
        "-DatabaseName", $DatabaseName
    )

    if ($NoPublish) {
        $extra += "-NoPublish"
    } else {
        # Publish once, then reuse
        if ($published) { $extra += "-NoPublish" }
    }

    if ($DoFindCeiling) {
        & (Join-Path $ScriptDir "Run-SpacetimeDBCeilingSweep.ps1") `
            -FindCeiling -Step $SpacetimeStep -MaxPlayers $SpacetimeMaxPlayers `
            -Duration $DurationSeconds -MaxErrRate $MaxErrRate -MaxLatencyMs $MaxLatencyMs `
            @extra
    } else {
        # If you want explicit counts, extend this wrapper; for now the wrapper uses ceiling search by default.
        throw "Run-SpacetimeDBSweepOnce: explicit-count mode not implemented; use -FindSpacetimeDBCeiling."
    }

    $script:published = $true
}

function Get-PassFromArcaneCsv([string] $CsvPath, [int] $NumServers, [int] $PlayersTotal) {
    if (-not (Test-Path $CsvPath)) { return $null }
    $rows = Import-Csv -Path $CsvPath -ErrorAction SilentlyContinue
    if (-not $rows) { return $null }
    $match = $rows | Where-Object { $_.num_servers -eq $NumServers -and $_.players -eq $PlayersTotal } | Select-Object -Last 1
    if (-not $match) { return $null }
    return ($match.pass -eq 'True' -or $match.pass -eq 'true')
}

function Run-ArcaneOnce([int] $NumServers, [int] $PlayersTotal) {
    Stop-ArcaneProcesses

    $args = @(
        "-NumServers", $NumServers,
        "-PlayersTotal", $PlayersTotal,
        "-Duration", $DurationSeconds,
        "-MaxErrRate", $MaxErrRate,
        "-MaxLatencyMs", $MaxLatencyMs,
        "-OutCsv", $ArcaneOutCsv,
        "-LogDir", $ArcaneLogDir,
        "-SpacetimeHost", $SpacetimeHost,
        "-DatabaseName", $DatabaseName,
        "-PersistBatchSize", $PersistBatchSize
    )

    if ($NoPublish -or $published) {
        $args += "-NoPublish"
    }

    & (Join-Path $ScriptDir "Run-ArcaneScalingSweep.ps1") @args
    $script:published = $true
}

if ($FindSpacetimeDBCeiling) {
    Write-Host "`n--- SpacetimeDB-only ceiling sweep ---" -ForegroundColor Cyan
    Run-SpacetimeDBSweepOnce -DoFindCeiling
} else {
    throw "Non-ceiling mode for SpacetimeDB-only not implemented yet; use -FindSpacetimeDBCeiling."
}

if ($FindArcaneCeiling) {
    Write-Host "`n--- Arcane+SpacetimeDB ceiling sweeps ---" -ForegroundColor Cyan
    foreach ($n in $ArcaneClusterCounts) {
        Write-Host "`n[Arcane ceiling] num_servers=$n" -ForegroundColor Yellow
        $players = $ArcaneCeilingStartPlayers
        $ceiling = $null
        while ($players -le $ArcaneCeilingMaxPlayers) {
            Write-Host "  Testing players=$players ..." -ForegroundColor Gray
            Run-ArcaneOnce -NumServers $n -PlayersTotal $players

            Start-Sleep -Seconds 1
            $pass = Get-PassFromArcaneCsv -CsvPath $ArcaneOutCsv -NumServers $n -PlayersTotal $players
            if ($pass -eq $null) {
                throw "Could not find pass/fail row for num_servers=$n players=$players in $ArcaneOutCsv"
            }

            if ($pass) {
                $ceiling = $players
                $players += $ArcaneCeilingStep
            } else {
                break
            }
        }

        Write-Host "  -> ceiling for num_servers=$n: $ceiling" -ForegroundColor Green
    }
} else {
    throw "Non-ceiling mode for Arcane+SpacetimeDB not implemented yet; use -FindArcaneCeiling."
}

Stop-ArcaneProcesses

Write-Host "`n=== DONE ===" -ForegroundColor Cyan
Write-Host "SpacetimeDB CSV: $SpacetimeOutCsv"
Write-Host "Arcane CSV: $ArcaneOutCsv"

