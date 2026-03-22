<#
.SYNOPSIS
  Run benchmark v2 (containerized, resource-limited) and write ceiling CSV.

.DESCRIPTION
  Uses docker compose for core services and launches Arcane cluster containers dynamically.
  This is a new benchmark profile (v2), not directly comparable to legacy/native numbers.

  **Public reproducibility (no private `arcane/` submodule):** use `-UsePublishedImages` and set
  `-ArcaneInfraImage` / `-ArcaneSwarmImage` (or environment variables `ARCANE_INFRA_IMAGE` /
  `ARCANE_SWARM_IMAGE`) to published container images. See REPRODUCIBILITY.md and docs/CLOUD_BENCHMARK_AWS.md.
#>
param(
  [int]$StartPlayers = 250,
  [int]$StepPlayers = 250,
  [int]$MaxPlayers = 6000,
  [int]$DurationSeconds = 30,
  [double]$MaxErrRate = 0.01,
  [double]$MaxLatencyMs = 200,
  [int[]]$ArcaneClusterCounts = @(1,2,3,4,5,10),
  [string]$DatabaseName = 'arcane',
  # Reachable *from containers*; the v2 runner starts SpacetimeDB in Docker.
  [string]$SpacetimeHost = 'http://spacetimedb:3000',
  # Host CLI `spacetime publish` target (mapped Docker port).
  [string]$SpacetimePublishServer = 'http://127.0.0.1:3000',
  [string]$OutDir = '',

  # Use docker-compose.v2.repro.yml + pull published images (no `docker build` of Arcane/swarm).
  [switch]$UsePublishedImages,
  [string]$ArcaneInfraImage = '',
  [string]$ArcaneSwarmImage = '',
  # Override compose file (default: v2 build-from-submodule, or v2.repro when -UsePublishedImages).
  [string]$ComposeFile = '',

  # While the benchmark runs: print `docker stats --no-stream` every N seconds (0 = off). Useful with AWS SSM log streaming.
  [int]$DockerStatsLogIntervalSec = 0
)

$ErrorActionPreference = 'Stop'
$ScriptDir = $PSScriptRoot
$RepoRoot = Resolve-Path (Join-Path $ScriptDir '..\..')
if ([string]::IsNullOrWhiteSpace($OutDir)) {
  $OutDir = Join-Path $ScriptDir ('v2_runs_' + (Get-Date -Format 'yyyyMMdd_HHmmss'))
}
$null = New-Item -ItemType Directory -Path $OutDir -Force

$resolvedInfra = $ArcaneInfraImage
if ([string]::IsNullOrWhiteSpace($resolvedInfra)) { $resolvedInfra = $env:ARCANE_INFRA_IMAGE }
$resolvedSwarm = $ArcaneSwarmImage
if ([string]::IsNullOrWhiteSpace($resolvedSwarm)) { $resolvedSwarm = $env:ARCANE_SWARM_IMAGE }

if ($UsePublishedImages) {
  if ([string]::IsNullOrWhiteSpace($resolvedInfra) -or [string]::IsNullOrWhiteSpace($resolvedSwarm)) {
    throw 'UsePublishedImages requires ARCANE_INFRA_IMAGE and ARCANE_SWARM_IMAGE (env or -ArcaneInfraImage / -ArcaneSwarmImage). See REPRODUCIBILITY.md.'
  }
}

if (-not [string]::IsNullOrWhiteSpace($ComposeFile)) {
  if ([System.IO.Path]::IsPathRooted($ComposeFile)) {
    $compose = $ComposeFile
  } else {
    $compose = Join-Path $RepoRoot $ComposeFile
  }
} elseif ($UsePublishedImages) {
  $compose = Join-Path $RepoRoot 'docker-compose.v2.repro.yml'
} else {
  $compose = Join-Path $RepoRoot 'docker-compose.v2.yml'
}

# Image tag used by `docker run` for dynamic cluster nodes (must match manager/cluster binaries).
$clusterInfraImage = if ($UsePublishedImages) { $resolvedInfra } else { 'arcane-v2/infra:latest' }
$modulePath = Join-Path (Join-Path $RepoRoot 'spacetimedb_demo') 'spacetimedb'
$envFile = Join-Path $OutDir '.env.v2'
$metricsDir = Join-Path $OutDir 'metrics'
$logsDir = Join-Path $OutDir 'logs'
$null = New-Item -ItemType Directory -Path $metricsDir -Force
$null = New-Item -ItemType Directory -Path $logsDir -Force

$script:_dsTimer = $null

function Start-DockerStatsLogTimer([int]$Seconds) {
  if ($Seconds -le 0) { return }
  $sec = [Math]::Max(5, $Seconds)
  Stop-DockerStatsLogTimer
  $script:_dsTimer = New-Object System.Timers.Timer ($sec * 1000)
  $script:_dsTimer.AutoReset = $true
  $null = Register-ObjectEvent -InputObject $script:_dsTimer -EventName Elapsed -SourceIdentifier ArcaneBenchDockerStats -Action {
    Write-Host ''
    Write-Host "----- docker stats $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') -----" -ForegroundColor DarkCyan
    & docker stats --no-stream --format "table {{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}" 2>&1 |
      ForEach-Object { Write-Host $_ }
  }
  $script:_dsTimer.Start()
}

function Stop-DockerStatsLogTimer {
  if ($null -ne $script:_dsTimer) {
    $script:_dsTimer.Stop()
    $script:_dsTimer.Dispose()
    $script:_dsTimer = $null
  }
  Unregister-Event -SourceIdentifier ArcaneBenchDockerStats -ErrorAction SilentlyContinue
}

function Test-LocalPortOpen([int]$Port) {
  try {
    $c = [System.Net.Sockets.TcpClient]::new()
    $c.Connect('127.0.0.1', $Port)
    $c.Close()
    return $true
  } catch {
    return $false
  }
}

function Invoke-Compose([string]$ComposeArgs) {
  $tok = [System.Collections.Generic.List[string]]::new()
  $tok.Add('compose')
  $tok.Add('-f'); $tok.Add($compose)
  $tok.Add('--env-file'); $tok.Add($envFile)
  foreach ($w in ($ComposeArgs.Trim() -split '\s+')) {
    if ($w.Length -gt 0) { $tok.Add($w) }
  }
  & docker @($tok.ToArray())
  if ($LASTEXITCODE -ne 0) { throw "docker compose failed: $ComposeArgs" }
}

function Get-SwarmFinal([string]$t) {
  $m = [regex]::Matches($t, 'FINAL:\s*players=(\d+)\s+total_calls=(\d+)\s+total_oks=(\d+)\s+total_errs=(\d+)\s+lat_avg_ms=([\d.]+)')
  if ($m.Count -eq 0) { return $null }
  $x = $m[$m.Count-1]
  [PSCustomObject]@{
    players=[int]$x.Groups[1].Value
    total_calls=[long]$x.Groups[2].Value
    total_oks=[long]$x.Groups[3].Value
    total_errs=[long]$x.Groups[4].Value
    lat_avg_ms=[double]$x.Groups[5].Value
  }
}

function Test-Pass($p){
  if (-not $p) { return $false }
  $err = if ($p.total_calls -gt 0) { $p.total_errs / $p.total_calls } else { 1.0 }
  return ($err -lt $MaxErrRate -and $p.lat_avg_ms -lt $MaxLatencyMs)
}

function Build-ClusterConfig([int]$n) {
  $ids = @()
  $entries = @()
  for($i=0; $i -lt $n; $i++) {
    $ids += ([guid]::NewGuid().ToString())
    $entries += "$($ids[$i]):arcane-v2-cluster-$($i):$([int](8090+$i))"
  }
  [PSCustomObject]@{
    Ids = $ids
    ManagerClusters = ($entries -join ',')
  }
}

function Write-EnvForManager([string]$ManagerClusters) {
  $lines = @(
    "MANAGER_CLUSTERS=$ManagerClusters",
    'NEIGHBOR_IDS_1=',
    'NEIGHBOR_IDS_2=',
    'NEIGHBOR_IDS_3='
  )
  if ($UsePublishedImages) {
    $lines += "ARCANE_INFRA_IMAGE=$resolvedInfra"
    $lines += "ARCANE_SWARM_IMAGE=$resolvedSwarm"
  }
  $lines | Set-Content $envFile
}

function Start-ClusterContainers([string[]]$Ids, [int]$NumServers, [string]$InfraImage) {
  $names = @()
  for($i=0; $i -lt $NumServers; $i++) {
    $name = "arcane-v2-cluster-$i"
    $neighbors = @()
    for($j=0; $j -lt $NumServers; $j++) {
      if ($j -ne $i) { $neighbors += $Ids[$j] }
    }
    $neighborStr = ($neighbors -join ',')
    docker rm -f $name 2>$null | Out-Null
    & docker run -d --name $name --network arcane-v2-net --cpus 1 --memory 2g `
      -e "CLUSTER_ID=$($Ids[$i])" -e "CLUSTER_WS_PORT=$([int](8090+$i))" -e 'REDIS_URL=redis://redis:6379' -e "NEIGHBOR_IDS=$neighborStr" `
      $InfraImage arcane-cluster
    if ($LASTEXITCODE -ne 0) { throw "failed to start cluster container $name" }
    $names += $name
  }
  return $names
}

function Stop-ClusterContainers([string[]]$Names) {
  foreach($n in $Names) {
    docker rm -f $n 2>$null | Out-Null
  }
}

function Capture-Stats([string]$ScenarioTag, [int]$Players, [int]$NumServers) {
  $outPath = Join-Path $metricsDir "docker_stats.csv"
  $line = docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.NetIO}},{{.BlockIO}}' 2>&1
  $ts = (Get-Date).ToString('o')
  foreach($row in $line) {
    if ([string]::IsNullOrWhiteSpace($row)) { continue }
    "$ts,$ScenarioTag,$NumServers,$Players,$row" | Add-Content $outPath
  }
}

function Dump-Logs([string]$ScenarioTag, [string[]]$ClusterNames) {
  $base = @('arcane-v2-redis','arcane-v2-manager') + $ClusterNames
  foreach($c in $base) {
    $p = Join-Path $logsDir ("$ScenarioTag`_$c.log")
    docker logs $c 2>&1 | Set-Content $p
  }
}

try {
  Start-DockerStatsLogTimer -Seconds $DockerStatsLogIntervalSec

  Write-EnvForManager ''
  if ($UsePublishedImages) {
    Invoke-Compose 'pull manager swarm'
  } else {
    Invoke-Compose 'build manager swarm'
  }

  # Spacetime-only infra
  Invoke-Compose 'up -d redis spacetimedb'

  # Wait for SpacetimeDB to be reachable on the host (used by `spacetime publish` CLI).
  $spOk = $false
  for($i=0; $i -lt 120; $i++){
    if (Test-LocalPortOpen -Port 3000) {
      $spOk = $true
      break
    }
    Start-Sleep -Milliseconds 500
  }
  if (-not $spOk) { throw "SpacetimeDB container not reachable on 127.0.0.1:3000 after timeout." }

  # publish module from host CLI to host SpacetimeDB
  Push-Location $modulePath
  spacetime build 2>&1
  if ($LASTEXITCODE -ne 0) { throw 'spacetime build failed' }
  # -s avoids default cloud login prompts on CI/Linux (non-interactive).
  spacetime publish $DatabaseName -y --server $SpacetimePublishServer 2>&1
  if ($LASTEXITCODE -ne 0) { throw 'spacetime publish failed' }
  Pop-Location

  $results = @()

  # SpacetimeDB-only ceiling
  $ceil = $null
  for($p=$StartPlayers; $p -le $MaxPlayers; $p += $StepPlayers){
    Write-Host "[v2 spacetimedb] players=$p" -ForegroundColor Gray
    $out = & docker compose -f $compose --env-file $envFile run --rm --no-deps --entrypoint arcane-swarm swarm `
      --backend spacetimedb --server-physics --players $p --tick-rate 10 --aps 2 --read-rate 5 --mode spread `
      --duration $DurationSeconds --uri $SpacetimeHost --db $DatabaseName 2>&1
    ($out | Out-String) | Set-Content (Join-Path $logsDir ("spacetimedb_only_swarm_players_$p.log"))
    $parsed = Get-SwarmFinal ($out | Out-String)
    Capture-Stats -ScenarioTag 'spacetimedb_only' -Players $p -NumServers 0
    if (Test-Pass $parsed) { $ceil = $p } else { break }
  }
  Dump-Logs -ScenarioTag 'spacetimedb_only' -ClusterNames @()
  $results += [PSCustomObject]@{ backend='spacetimedb_only'; num_servers=0; ceiling_players=$ceil }

  foreach($n in $ArcaneClusterCounts) {
    Write-Host "Running Arcane+Spacetime v2 for clusters=$n" -ForegroundColor Cyan
    $cfg = Build-ClusterConfig $n
    Write-EnvForManager $cfg.ManagerClusters

    # Keep SpacetimeDB running; recreate Redis/manager to apply the new MANAGER_CLUSTERS env.
    Invoke-Compose 'up -d --force-recreate redis manager'

    $clusterNames = Start-ClusterContainers -Ids $cfg.Ids -NumServers $n -InfraImage $clusterInfraImage

    $ceilA = $null
    for($p=$StartPlayers; $p -le $MaxPlayers; $p += $StepPlayers){
      Write-Host "[v2 arcane n=$n] players=$p" -ForegroundColor Gray
      $out = & docker compose -f $compose --env-file $envFile run --rm --no-deps --entrypoint arcane-swarm swarm `
        --backend arcane --players $p --tick-rate 10 --aps 2 --read-rate 5 --mode spread --duration $DurationSeconds `
        --arcane-manager http://manager:8081 --uri $SpacetimeHost --db $DatabaseName 2>&1
      ($out | Out-String) | Set-Content (Join-Path $logsDir ("arcane_n${n}_swarm_players_$p.log"))
      $parsed = Get-SwarmFinal ($out | Out-String)
      Capture-Stats -ScenarioTag 'arcane_plus_spacetimedb' -Players $p -NumServers $n
      if (Test-Pass $parsed) { $ceilA = $p } else { break }
    }

    Dump-Logs -ScenarioTag ("arcane_n$n") -ClusterNames $clusterNames
    Stop-ClusterContainers -Names $clusterNames
    $results += [PSCustomObject]@{ backend='arcane_plus_spacetimedb'; num_servers=$n; ceiling_players=$ceilA }
  }

  $csv = Join-Path $OutDir 'benchmark_v2_results.csv'
  $results | Export-Csv -NoTypeInformation -Path $csv
  Write-Host "v2 results written: $csv" -ForegroundColor Green
  $results | Format-Table -AutoSize
}
finally {
  Stop-DockerStatsLogTimer
  try { Invoke-Compose 'down --remove-orphans' } catch {}
  for($i=0; $i -lt 12; $i++){ docker rm -f "arcane-v2-cluster-$i" 2>$null | Out-Null }
}
