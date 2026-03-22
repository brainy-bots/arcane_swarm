# Reproducing the experiment

This document gives step-by-step instructions to run the same scaling experiments on your machine. The repository is self-contained: it includes the orchestration scripts and vendors the SpacetimeDB module source and swarm runtime code.

**Two paths:**

| Path | Submodules | Use case |
|------|------------|----------|
| **Public / third party** | None required for **Benchmark v2** | Use **published** Arcane infra + swarm container images (`-UsePublishedImages`). Clone the **public** benchmark repo only. |
| **From source (developers)** | **`arcane/`** (and optionally **`arcane-demos/`** for legacy scripts) | Build manager/cluster locally or via `docker-compose.v2.yml` `build:` |

Legacy/native sweeps (`scripts/swarm/*`) still build the swarm crate and may expect `arcane/` on disk for Arcane+Spacetime runs — see sections 3–4.

---

## Prerequisites

- **Rust** (stable): [rustup.rs](https://rustup.rs)
- **Redis**: running locally, default `redis://127.0.0.1:6379`
- **SpacetimeDB**: [Install the CLI](https://spacetimedb.com/docs).
  - For `scripts/swarm/*` sweeps, run `spacetime start` in a separate terminal (must be running before the scripts publish the module and during runs).
  - For `scripts/benchmark/Run-Benchmark-V2.ps1`, the SpacetimeDB server is started automatically in Docker; the runner still uses the host `spacetime` CLI to build/publish the module to `localhost:3000`.
- **wasm-opt** (recommended): SpacetimeDB uses it to optimize the WASM module. **Without it, the module runs unoptimized and your ceiling numbers can be lower than the documented results.**  
  - **Windows:** Run once: `.\scripts\Install-WasmOpt.ps1` (downloads Binaryen and adds it to PATH for that session). Or download [binaryen](https://github.com/WebAssembly/binaryen/releases) manually, extract it, and add the `bin` folder to your PATH.
- **PowerShell**: scripts are written for Windows PowerShell.

**Full benchmark runtime:** The one-command full benchmark can run **30+ minutes**. Run it in a **separate PowerShell window** (e.g. open a new terminal outside your IDE) so your editor does not freeze. The script will print progress as it runs.

---

## 1. Clone the repository (and submodules if you build Arcane from source)

**Public benchmark + v2 with published images only:**

```powershell
git clone https://github.com/martinjms/arcane-scaling-benchmarks.git
cd arcane-scaling-benchmarks
# No submodules. Set ARCANE_INFRA_IMAGE / ARCANE_SWARM_IMAGE and use Run-Benchmark-V2.ps1 -UsePublishedImages (section 10).
```

**Developers building Arcane binaries locally / docker `build` for v2:**

```powershell
git clone --recurse-submodules https://github.com/martinjms/arcane-scaling-benchmarks.git
cd arcane-scaling-benchmarks
```

If you already cloned without `--recurse-submodules`:

```powershell
git submodule update --init --recursive
```

You should see `arcane/` populated where required for those scripts.

---

## 2. Start Redis and SpacetimeDB

- **Redis:** Start your Redis server (e.g. `redis-server` or Windows service).
- **SpacetimeDB:** In a separate terminal, run:

  ```powershell
  spacetime start
  ```

  Leave it running. The scripts will build and publish the module to it (unless you pass `-NoPublish`).

---

## 3. SpacetimeDB-only ceiling sweep

Finds the maximum player count for a single SpacetimeDB module (server-physics).

```powershell
cd scripts\swarm
.\Run-SpacetimeDBCeilingSweep.ps1 -FindCeiling -Step 250 -MaxPlayers 2000
```

Or specific player counts:

```powershell
.\Run-SpacetimeDBCeilingSweep.ps1 -PlayerCounts 250,500,750,1000,1250
```

Results are appended to `spacetimedb_ceiling_sweep.csv` in the same directory. First run will build/publish the vendored SpacetimeDB module and build the benchmark swarm runtime (`crates/arcane-benchmark-swarm`).

---

## 4. Arcane + SpacetimeDB scaling sweep

Runs N cluster processes, the manager, and the swarm. Use **no batch cap** (default) for the reported ceilings.

```powershell
.\Run-ArcaneScalingSweep.ps1 -NumServers 2 -PlayersTotal 1000
```

Examples:

- 1 cluster, 500 players:  
  `.\Run-ArcaneScalingSweep.ps1 -NumServers 1 -PlayersTotal 500`
- 3 clusters, 4000 players (no cap, recommended):  
  `.\Run-ArcaneScalingSweep.ps1 -NumServers 3 -PlayersTotal 4000`
- With persist batch cap 500 (for comparison):  
  `.\Run-ArcaneScalingSweep.ps1 -NumServers 3 -PlayersTotal 4000 -PersistBatchSize 500`

Results are appended to `arcane_scaling_sweep.csv`. Logs (manager and per-cluster) go to `arcane_scaling_logs/`. The first run builds:
- `crates/arcane-benchmark-swarm` (for `arcane-swarm`)
- `arcane/` (for `arcane-manager` and `arcane-cluster`)

---

## 5. Optional parameters

- **Run-ArcaneScalingSweep.ps1:**  
  `-NoPublish` (skip SpacetimeDB build/publish), `-OutCsv`, `-LogDir`, `-SpacetimeHost`, `-DatabaseName`, `-PersistBatchSize` (default 0).
- **Run-SpacetimeDBCeilingSweep.ps1:**  
  `-NoPublish`, `-OutCsv`, `-RepeatCount`, `-CooldownSeconds`, `-SpacetimeHost`, `-DatabaseName`.

---

## 6. Single run at a time

Do not run multiple sweeps in parallel on the same machine. They use fixed ports (manager 8081, clusters 8090, 8091, …). The scripts stop any existing `arcane-*` processes before starting; run one configuration to completion before starting another.

---

## 7. Canonical parameters

All runs use the same workload and pass criteria. See [docs/CANONICAL_PARAMETERS.md](docs/CANONICAL_PARAMETERS.md).

## 8. One-command full benchmark

For an end-to-end run (SpacetimeDB-only ceiling + Arcane+SpacetimeDB ceilings for multiple cluster counts), run:

```powershell
cd scripts\swarm
.\Run-FullBenchmark-Incremental.ps1
```

(or `.\Run-FullBenchmark.ps1`; both delegate to `Run-Benchmark-Scenarios.ps1`). The script prints a **ceiling summary** at the end and writes `benchmark_scenarios_results.csv` (columns: `backend`, `num_servers`, `ceiling_players`).

---

## 9. Comparing your numbers with documentation

After a run, compare the printed ceiling summary (or the CSV) with the results documented in the **arcane-demos** repo:

- **SpacetimeDB only:** Ceiling **1,000 players** (1,250 fails on latency). See `arcane-demos/docs/SCALING_EXPERIMENT_RESULTS.md` §1.
- **Arcane + SpacetimeDB:** Reference ceilings (with `PersistBatchSize=0`): 1 cluster ≥1,750; 2 clusters 3,000; 3 clusters 3,000–5,000; 4 clusters 4,000; 5 clusters 4,000; 10 clusters 5,500. See `SCALING_EXPERIMENT_RESULTS.md` §§2–5.

Same canonical parameters and pass criteria (err_rate &lt; 1%, lat_avg_ms &lt; 200) are used. Hardware and OS will affect the exact numbers; the doc gives the reference run for comparison.


## 10. Benchmark v2 (containerized)

Method details and caveats: [docs/BENCHMARK_V2_METHOD.md](docs/BENCHMARK_V2_METHOD.md).

### 10a. Reproducible path (no `arcane/` submodule)

Use **published** images (same tags you document for the experiment). Example:

```powershell
cd scripts\benchmark
$env:ARCANE_INFRA_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-infra:v1.0.0'
$env:ARCANE_SWARM_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-swarm:v1.0.0'
.\Run-Benchmark-V2.ps1 -UsePublishedImages
```

Compose file used: `docker-compose.v2.repro.yml`. See `.env.v2.repro.example` for the variable names.

### 10b. Developer path (build from `arcane/` submodule)

```powershell
cd scripts\benchmark
.\Run-Benchmark-V2.ps1
```

This builds images via `docker-compose.v2.yml` (requires `arcane/` present).

## 11. Benchmark v2 in AWS (ephemeral one-command runner)

Requires **public** benchmark repo clone and **published** image tags (no GitHub token on the instance):

```powershell
cd scripts\cloud
$env:ARCANE_INFRA_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-infra:v1.0.0'
$env:ARCANE_SWARM_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-swarm:v1.0.0'
.\Run-Benchmark-V2-Aws.ps1 -ArtifactBucket <your-s3-bucket> -Region us-east-1
```

The script provisions temporary AWS resources, runs v2 remotely, downloads outputs locally, and tears everything down by default. It **waits until the run finishes** (full sweeps can take a long time), then prints a **results table** in the terminal and leaves files under `scripts/cloud/aws_runs_*` and in S3.

Full details: [docs/CLOUD_BENCHMARK_AWS.md](docs/CLOUD_BENCHMARK_AWS.md).

To create the artifact bucket with Terraform: [infra/aws/terraform/README.md](infra/aws/terraform/README.md).

