<#
.SYNOPSIS
  Build and push Benchmark v2 Docker images (local alternative to GitHub Actions).

.DESCRIPTION
  From repo root, with `arcane/` populated (submodule), builds infra + swarm and pushes to a registry.
  Log in first, e.g. `docker login ghcr.io` (GitHub PAT with write:packages).

.EXAMPLE
  .\scripts\docker\Publish-Images.ps1 -Owner martinjms -Tag v1.0.0
#>
param(
  [string]$Registry = 'ghcr.io',
  [Parameter(Mandatory = $true)]
  [string]$Owner,
  [string]$Tag = 'latest'
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $RepoRoot

$ownerLc = $Owner.ToLowerInvariant()
$infra = "$Registry/${ownerLc}/arcane-benchmark-infra:$Tag"
$swarm = "$Registry/${ownerLc}/arcane-benchmark-swarm:$Tag"

if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'arcane/Cargo.toml'))) {
  throw 'arcane/ missing. Run: git submodule update --init --recursive'
}

Write-Host "Building $infra ..." -ForegroundColor Cyan
docker build -f docker/Dockerfile.arcane-infra -t $infra .
if ($LASTEXITCODE -ne 0) { throw 'infra build failed' }

Write-Host "Building $swarm ..." -ForegroundColor Cyan
docker build -f docker/Dockerfile.swarm -t $swarm .
if ($LASTEXITCODE -ne 0) { throw 'swarm build failed' }

Write-Host "Pushing..." -ForegroundColor Cyan
docker push $infra
if ($LASTEXITCODE -ne 0) { throw 'infra push failed' }
docker push $swarm
if ($LASTEXITCODE -ne 0) { throw 'swarm push failed' }

Write-Host "Done:" -ForegroundColor Green
Write-Host "  $infra"
Write-Host "  $swarm"
Write-Host "Set packages to Public on GHCR if reproducers should pull without login." -ForegroundColor Yellow
