# Benchmark v2 Method (Containerized)

This profile runs benchmark components in Docker containers with explicit CPU/memory limits.
It is intended as an intermediate step toward multi-host/AWS deployment and bottleneck analysis.

## Goals

- Reproducible one-command benchmark run.
- Resource isolation per component.
- Clear migration path to one-service-per-host deployment.
- Capture per-service utilization snapshots and logs for bottleneck attribution.

## Topology
- `spacetimedb` container (`${SPACETIME_IMAGE:-clockworklabs/spacetime:2.0.5}`)
- `redis` container (`redis:7-alpine`)
- `arcane-manager` container (built from `arcane/`)
- `arcane-cluster` containers (dynamically started by runner; supports 1..10 clusters)
- `arcane-swarm` container (built from `crates/arcane-benchmark-swarm`)

## Resource limits (defaults)

- `spacetimedb`: 2 CPU, 4GB RAM
- `redis`: 1 CPU, 1GB RAM
- `manager`: 1 CPU, 1GB RAM
- each `cluster`: 1 CPU, 2GB RAM (set by runner `docker run` flags)

Tune limits in `docker-compose.v2.yml` (core services) and `Run-Benchmark-V2.ps1` (cluster containers).

## Run

```powershell
cd scripts/benchmark
./Run-Benchmark-V2.ps1
```

Outputs:

- `benchmark_v2_results.csv` (ceiling by scenario)
- `metrics/docker_stats.csv` (snapshot utilization per step)
- `logs/*.log` (per-container logs by scenario)

## Important caveats

- v2 numbers are **not directly comparable** to legacy/native README numbers.
- SpacetimeDB module publish is done via host `spacetime` CLI targeting containerized SpacetimeDB on `localhost:3000`.

## Future step (AWS)

Use same service boundaries to deploy one container group per host (manager, each cluster, spacetimedb, redis, swarm runner), then collect host-level + app-level metrics to identify bottlenecks.
