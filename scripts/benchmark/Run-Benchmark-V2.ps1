<#
.SYNOPSIS
  Run benchmark v2 (containerized, resource-limited) and write ceiling CSV.

.DESCRIPTION
  Uses docker compose for core services and launches Arcane cluster containers dynamically.
  This is a new benchmark profile (v2), not directly comparable to legacy/native numbers.
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
  [string]$SpacetimeHost = 'http://127.0.0.1:3000',
  [string]$OutDir = '',
  # When set, skip `docker compose build` and use existing local tags (e.g. arcane-v2/infra:latest from GHCR on EC2).
  [switch]$SkipImageBuild
)

$ErrorActionPreference = 'Stop'
$ScriptDir = $PSScriptRoot
$RepoRoot = Resolve-Path (Join-Path $ScriptDir '..\..')
if ([string]::IsNullOrWhiteSpace($OutDir)) {
  $OutDir = Join-Path $ScriptDir ('v2_runs_' + (Get-Date -Format 'yyyyMMdd_HHmmss'))
}
$null = New-Item -ItemType Directory -Path $OutDir -Force
$compose = Join-Path $RepoRoot 'docker-compose.v2.yml'
$modulePath = Join-Path $RepoRoot 'spacetimedb_demo\spacetimedb'
$envFile = Join-Path $OutDir '.env.v2'
$metricsDir = Join-Path $OutDir 'metrics'
$logsDir = Join-Path $OutDir 'logs'
$null = New-Item -ItemType Directory -Path $metricsDir -Force
$null = New-Item -ItemType Directory -Path $logsDir -Force

function Invoke-Compose([string]$ComposeArgs) {
  cmd /c "docker compose -f `"$compose`" --env-file `"$envFile`" $ComposeArgs"
  if ($LASTEXITCODE -ne 0) { throw "docker compose failed: $ComposeArgs" }
}

function Get-SwarmFinal([string]$t) {
  $m = [regex]::Matches($t, 'FINAL:\s*players=(\d+)\s+total_calls=(\d+)\s+total_oks=(\d+)\s+total_errs=(\d+)\s+lat_avg_ms=([\d.]+)')
  if ($m.Count -eq 0) { return $null }
  $x = $m[$m.Count-1]
  $s = [regex]::Matches($t, 'FINAL_SPACETIMEDB:\s*action_calls=(\d+)\s+action_oks=(\d+)\s+action_errs=(\d+)')
  $actionCalls = 0L
  $actionErrs = 0L
  if ($s.Count -gt 0) {
    $sx = $s[$s.Count-1]
    $actionCalls = [long]$sx.Groups[1].Value
    $actionErrs = [long]$sx.Groups[3].Value
  }
  [PSCustomObject]@{
    players=[int]$x.Groups[1].Value
    total_calls=[long]$x.Groups[2].Value
    total_oks=[long]$x.Groups[3].Value
    total_errs=[long]$x.Groups[4].Value
    lat_avg_ms=[double]$x.Groups[5].Value
    action_calls=$actionCalls
    action_errs=$actionErrs
  }
}

function Test-Pass($p){
  if (-not $p) { return $false }
  $calls = $p.total_calls + $p.action_calls
  $errs = $p.total_errs + $p.action_errs
  $err = if ($calls -gt 0) { $errs / $calls } else { 1.0 }
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
  @(
    "MANAGER_CLUSTERS=$ManagerClusters",
    'NEIGHBOR_IDS_1=',
    'NEIGHBOR_IDS_2=',
    'NEIGHBOR_IDS_3='
  ) | Set-Content $envFile
}

function Start-ClusterContainers([string[]]$Ids, [int]$NumServers) {
  $names = @()
  for($i=0; $i -lt $NumServers; $i++) {
    $name = "arcane-v2-cluster-$i"
    $neighbors = @()
    for($j=0; $j -lt $NumServers; $j++) {
      if ($j -ne $i) { $neighbors += $Ids[$j] }
    }
    $neighborStr = ($neighbors -join ',')
    cmd /c "docker rm -f $name 2>nul"
    cmd /c "docker run -d --name $name --network arcane-v2-net --cpus 1 --memory 2g -e CLUSTER_ID=$($Ids[$i]) -e CLUSTER_WS_PORT=$([int](8090+$i)) -e REDIS_URL=redis://redis:6379 -e NEIGHBOR_IDS=$neighborStr arcane-v2/infra:latest arcane-cluster"
    if ($LASTEXITCODE -ne 0) { throw "failed to start cluster container $name" }
    $names += $name
  }
  return $names
}

function Stop-ClusterContainers([string[]]$Names) {
  foreach($n in $Names) {
    cmd /c "docker rm -f $n 2>nul" | Out-Null
  }
}

function Capture-Stats([string]$ScenarioTag, [int]$Players, [int]$NumServers) {
  $outPath = Join-Path $metricsDir "docker_stats.csv"
  $line = cmd /c "docker stats --no-stream --format `"{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.NetIO}},{{.BlockIO}}`""
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
    cmd /c "docker logs $c 2>&1" | Set-Content $p
  }
}

try {
  $spOk = (Test-NetConnection -ComputerName 127.0.0.1 -Port 3000 -WarningAction SilentlyContinue).TcpTestSucceeded
  if (-not $spOk) { throw "SpacetimeDB host service not reachable on 127.0.0.1:3000. Start it before running v2." }

  # initial env + images (build locally, or use pre-tagged images e.g. after docker pull on cloud)
  Write-EnvForManager ''
  if ($SkipImageBuild) {
    Write-Host 'SkipImageBuild: using existing arcane-v2/infra:latest and arcane-v2/swarm:latest' -ForegroundColor Yellow
  } else {
    Invoke-Compose 'build manager swarm'
  }

  # Spacetime-only infra
  Invoke-Compose 'up -d redis'

  # publish module from host CLI to host SpacetimeDB
  Push-Location $modulePath
  cmd /c 'spacetime build 2>&1'
  if ($LASTEXITCODE -ne 0) { throw 'spacetime build failed' }
  cmd /c "spacetime publish $DatabaseName --yes 2>&1"
  if ($LASTEXITCODE -ne 0) { throw 'spacetime publish failed' }
  Pop-Location

  $results = @()

  # SpacetimeDB-only ceiling
  $ceil = $null
  for($p=$StartPlayers; $p -le $MaxPlayers; $p += $StepPlayers){
    Write-Host "[v2 spacetimedb] players=$p" -ForegroundColor Gray
    $out = cmd /c "docker compose -f `"$compose`" --env-file `"$envFile`" run --rm --no-deps --entrypoint arcane-swarm swarm --backend spacetimedb --server-physics --players $p --tick-rate 10 --aps 2 --read-rate 5 --mode spread --duration $DurationSeconds --uri http://host.docker.internal:3000 --db $DatabaseName 2>&1"
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

    Invoke-Compose 'down --remove-orphans'
    Invoke-Compose 'up -d redis manager'

    $clusterNames = Start-ClusterContainers -Ids $cfg.Ids -NumServers $n

    $ceilA = $null
    for($p=$StartPlayers; $p -le $MaxPlayers; $p += $StepPlayers){
      Write-Host "[v2 arcane n=$n] players=$p" -ForegroundColor Gray
      $out = cmd /c "docker compose -f `"$compose`" --env-file `"$envFile`" run --rm --no-deps --entrypoint arcane-swarm swarm --backend arcane --server-physics --players $p --tick-rate 10 --aps 2 --read-rate 5 --mode spread --duration $DurationSeconds --arcane-manager http://manager:8081 --uri http://host.docker.internal:3000 --db $DatabaseName 2>&1"
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
  try { Invoke-Compose 'down --remove-orphans' } catch {}
  for($i=0; $i -lt 12; $i++){ cmd /c "docker rm -f arcane-v2-cluster-$i 2>nul" | Out-Null }
}
