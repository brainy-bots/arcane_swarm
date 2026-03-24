param(
  [Parameter(Mandatory = $true)]
  [string]$LogsDir
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $LogsDir)) {
  throw "Logs directory not found: $LogsDir"
}

$files = Get-ChildItem -LiteralPath $LogsDir -File -Filter '*swarm_players_*.log'
if ($files.Count -eq 0) {
  throw "No swarm log files found in: $LogsDir"
}

function Parse-Line([string]$line, [string]$kind) {
  if ($kind -eq 'final') {
    if ($line -match 'FINAL:\s*players=(\d+)\s+total_calls=(\d+)\s+total_oks=(\d+)\s+total_errs=(\d+)\s+lat_avg_ms=([\d.]+)') {
      return @{
        players = [int]$matches[1]
        total_calls = [long]$matches[2]
        total_errs = [long]$matches[4]
        lat_avg_ms = [double]$matches[5]
      }
    }
  }
  if ($kind -eq 'action') {
    if ($line -match 'FINAL_SPACETIMEDB:\s*action_calls=(\d+)\s+action_oks=(\d+)\s+action_errs=(\d+)') {
      return @{
        action_calls = [long]$matches[1]
        action_errs = [long]$matches[3]
      }
    }
  }
  return $null
}

$rows = foreach ($f in $files) {
  $content = Get-Content -LiteralPath $f.FullName
  $final = $null
  $action = @{ action_calls = 0L; action_errs = 0L }
  foreach ($line in $content) {
    $parsedFinal = Parse-Line -line $line -kind 'final'
    if ($parsedFinal) { $final = $parsedFinal }
    $parsedAction = Parse-Line -line $line -kind 'action'
    if ($parsedAction) { $action = $parsedAction }
  }
  if (-not $final) { continue }

  $reportedRate = if ($final.total_calls -gt 0) { [math]::Round((100.0 * $final.total_errs) / $final.total_calls, 3) } else { 100.0 }
  $effectiveCalls = $final.total_calls + $action.action_calls
  $effectiveErrs = $final.total_errs + $action.action_errs
  $effectiveRate = if ($effectiveCalls -gt 0) { [math]::Round((100.0 * $effectiveErrs) / $effectiveCalls, 3) } else { 100.0 }

  [PSCustomObject]@{
    file = $f.Name
    players = $final.players
    reported_calls = $final.total_calls
    reported_errs = $final.total_errs
    reported_err_pct = $reportedRate
    action_calls = $action.action_calls
    action_errs = $action.action_errs
    effective_err_pct = $effectiveRate
    lat_avg_ms = $final.lat_avg_ms
  }
}

$rows |
  Sort-Object file |
  Format-Table -AutoSize
