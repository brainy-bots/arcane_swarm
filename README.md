# Arcane Scaling Benchmarks: Experiment Report

**A reproducible study of player-capacity ceilings for a distributed game-simulation stack (Arcane clusters + SpacetimeDB) under a fixed workload.**

---

## Abstract

We report results from a controlled scaling experiment on a single machine. The system under test is a multiplayer simulation backend in two configurations: (1) **SpacetimeDB only**, with physics and persistence in a single module, and (2) **Arcane plus SpacetimeDB**, where multiple Arcane cluster processes run physics, replicate entity state via Redis, and periodically persist to SpacetimeDB. A headless swarm client drives a canonical workload (10 Hz position updates, 2 actions/s, 30 s, everyone-sees-everyone). We measure client-observed error rate and latency and determine the maximum concurrent player count (ceiling) for which the system stays below 1% error rate and 200 ms average latency.

**Findings:** SpacetimeDB-only ceiling is **1,000 players** (failure at 1,250 is latency-driven). Arcane+SpacetimeDB ceilings depend on cluster count and on the size of persist batches: with a single large persist request per second, we observe **3 clusters sustaining 5,000 players**, **5 clusters sustaining 4,000**, and **10 clusters sustaining 5,500 players**. Introducing a 500-entity cap on persist batch size (multiple HTTP requests per second) increased persist duration and **reduced** the observed ceilings (e.g. 3 clusters at 4,000 players failed with the cap and passed without it). On fixed hardware, adding clusters distributes load but multiplies cross-cluster replication; the ceiling is determined by the tradeoff between per-cluster load and replication cost.

---

## 1. Introduction

### 1.1 Objective

To quantify, under a fixed workload and pass/fail criteria, the maximum number of concurrent simulated players (ceiling) for:

- A **SpacetimeDB-only** backend (single module, server-authoritative physics and persistence).
- An **Arcane + SpacetimeDB** backend (multiple Arcane cluster processes, Redis replication, periodic batch persist to SpacetimeDB).

No claim is made about real-world deployments (network, multiple machines, or different workloads). The experiment is single-machine, headless, and reproducible with the described setup.

### 1.2 Scope

- **Workload:** Deterministic, synthetic client behavior (position updates and actions at fixed rates; no Unreal or other game client).
- **Metrics:** Client-side success/error counts and average latency for RPC/send operations; server-side tick duration and persist duration where available.
- **Environment:** One physical (or virtual) host; Redis and SpacetimeDB running locally; all Arcane cluster processes and the swarm on the same host.

---

## 2. Experimental Design

### 2.1 Research Questions

1. What is the player ceiling for SpacetimeDB-only under the canonical workload?
2. How does the ceiling change when the same workload is served by 1, 2, 3, 4, 5, and 10 Arcane clusters with SpacetimeDB persistence?
3. Does capping the number of entities per SpacetimeDB persist request (batch size) improve or worsen the observed ceiling?

### 2.2 Pass Criteria

A run is **passed** if and only if:

- **Error rate** (client-observed failed or timed-out operations) **< 1%**
- **Average latency** (client-observed, in ms) **< 200 ms**

The ceiling for a given configuration is the largest player count for which at least one run passes.

### 2.3 Canonical Workload Parameters

All runs use the same workload so that SpacetimeDB-only and Arcane+SpacetimeDB results are comparable.

| Parameter | Value | Description |
|-----------|--------|-------------|
| Tick rate | 10 Hz | Position updates per second per player |
| Actions per second | 2 | Actions (e.g. interact) per second per player |
| Duration | 30 s | Steady-state phase (additional warmup may apply) |
| Mode | spread | Movement pattern |
| Visibility | everyone-sees-everyone | All clients receive all entity positions |
| Demo entities | 0 | No NPCs; players only |
| SpacetimeDB persist rate | 1 Hz | (Arcane+Spacetime) Batch persist once per second |
| Redis | enabled | (Arcane+Spacetime) Replication between clusters when *N* > 1 |

---

## 3. Setup

### 3.1 Components

- **Swarm client:** Headless process that spawns *P* logical “players,” each performing the canonical workload (position updates and actions) and reporting success/failure and latency. For Arcane+Spacetime, each player resolves a cluster via a manager (round-robin) and connects to that cluster’s WebSocket.
- **SpacetimeDB:** Local instance; for SpacetimeDB-only runs, the module runs physics and persistence; for Arcane+Spacetime, the module receives batch persist calls from each Arcane cluster at 1 Hz.
- **Arcane clusters:** One process per cluster; each runs a tick loop, applies client updates, merges replicated state from neighbors, and optionally persists the merged view to SpacetimeDB. Clusters discover each other via a manager and replicate via Redis pub/sub.
- **Redis:** Used by Arcane for cross-cluster replication when the number of clusters is greater than one.

### 3.2 Harness and Scripts

This repository is self-contained: it includes the **orchestration scripts** (`scripts/swarm/Run-ArcaneScalingSweep.ps1`, `Run-SpacetimeDBCeilingSweep.ps1`) plus the benchmark swarm code and the SpacetimeDB module source. **Benchmark v2** (`scripts/benchmark/Run-Benchmark-V2.ps1`) can run from a **public clone only** using **published** Arcane infra + swarm container images — no submodule or private repo access ([REPRODUCIBILITY.md](REPRODUCIBILITY.md) §10a). Legacy/native sweeps still use the **`arcane/`** submodule to build manager/cluster binaries unless you only run SpacetimeDB-only steps. Step-by-step reproducibility is in [REPRODUCIBILITY.md](REPRODUCIBILITY.md).

### 3.3 Single-Machine Constraint

All processes run on one host. Adding clusters therefore **distributes** the same total player load across more processes but **increases** the total replication traffic (each cluster replicates to *N*−1 neighbors). The ceiling is not necessarily monotonic in *N*: it depends on the balance between per-cluster load and replication cost.

---

## 4. Results

### 4.1 SpacetimeDB Only

Single SpacetimeDB module; physics and persistence in the module.

| Players | total_calls | total_errs | err_rate_pct | lat_avg_ms | Pass |
|--------|-------------|------------|--------------|------------|------|
| 250    | 93,047      | 0          | 0            | 5.87      | Yes  |
| 500    | 192,308     | 0          | 0            | 91.68     | Yes  |
| 750    | 302,546     | 0          | 0            | 68.28     | Yes  |
| 1000   | 418,164     | 0          | 0            | 183.61    | Yes  |
| 1250   | 542,789     | 0          | 0            | **688.35** | **No** (latency) |

**Ceiling: 1,000 players.** Failure at 1,250 is due to latency exceeding the 200 ms threshold, not error rate.

---

### 4.2 Arcane + SpacetimeDB (Persist Batch Cap = 500)

Arcane clusters replicate via Redis; each cluster persists to SpacetimeDB at 1 Hz with **at most 500 entities per HTTP request** (multiple requests per persist window when entity count &gt; 500).

Representative results (passing runs first; then failing). Full sweep data are written to CSVs by the scripts in `scripts/swarm/`.

| Clusters | Players | err_rate_pct | lat_avg_ms | Pass |
|----------|---------|--------------|------------|------|
| 4        | 4000    | 0.08         | 2.41       | Yes  |
| 4        | 3000    | 0.05         | 0.02       | Yes  |
| 3        | 3000    | 0.02         | 0.04       | Yes  |
| 2        | 3000    | 0.67         | 0.61       | Yes  |
| 2        | 3500    | 1.08         | 1.44       | No   |
| 4        | 5000    | 7.66         | 25.82      | No   |
| 3        | 4000    | 5.76         | 17.58      | No   |
| 3        | 5000    | 16.08        | 26.13      | No   |
| 5        | 4000    | 15.95        | 25.05      | No   |
| 5        | 5000    | 2.96         | 9.40       | No   |

**Observed ceilings with cap = 500:** 1 cluster ≥1750; 2 clusters 3000; 3 clusters 3000; 4 clusters 4000; 5 clusters 3000.

---

### 4.3 Effect of Persist Batch Size (No Cap vs Cap 500)

We repeated selected configurations with **no cap** on persist batch size (single HTTP request per persist window).

| Clusters | Players | With cap 500   | With no cap    |
|----------|---------|----------------|----------------|
| 3        | 3000    | Pass (0.02%)   | Pass (0.11%)   |
| 3        | 4000    | **Fail (5.76%)** | **Pass (0%)**  |
| 3        | 5000    | Fail (16%)     | **Pass (0.03%)** |
| 5        | 4000    | Fail (15.95%)  | **Pass (0.49%)** |
| 4        | 5000    | Fail (7.66%)   | Fail (5.01%)   |
| 5        | 5000    | Fail (2.96%)   | Fail (16.05%)  |

**Discovery:** With the 500-entity cap, each cluster sends several sequential HTTP requests per persist window; total persist time often reached ~1–2 s and blocked the tick loop, increasing client errors. With no cap, a single large request often completed in ~200–800 ms. For this workload, **removing the batch cap improved ceilings** (e.g. 3 clusters at 4,000 and 5,000 pass without cap and fail with cap). We therefore use **no cap** (single request per persist) for the remaining reported runs.

---

### 4.4 Arcane + SpacetimeDB (No Cap) — Extended Sweep

| Clusters | Players | err_rate_pct | lat_avg_ms | Pass |
|----------|---------|--------------|------------|------|
| 3        | 4000    | 0            | 0          | Yes  |
| 3        | 5000    | 0.03         | 2.2        | Yes  |
| 5        | 4000    | 0.49         | 5.4        | Yes  |
| 4        | 5000    | 5.01         | 26.4       | No   |
| 5        | 5000    | 16.05        | 14.2       | No   |

---

### 4.5 Ten Clusters (No Cap) — Ceiling Sweep

With 10 clusters, per-cluster load is lower (fewer entities per cluster) but each cluster replicates to 9 neighbors. On the same hardware, load is distributed and replication is multiplied.

| Clusters | Players | err_rate_pct | lat_avg_ms | Pass |
|----------|---------|--------------|------------|------|
| 10       | 5000    | 0.03         | 13.8       | Yes  |
| 10       | 5500    | 0.95         | 30.8       | Yes  |
| 10       | 5750    | 1.58         | 22.2       | No   |
| 10       | 6000    | 4.43         | 19.8       | No   |

**10-cluster ceiling: 5,500 players** (5,500 passes at 0.95%; 5,750 fails at 1.58%).

---

## 5. Discussion

### 5.1 SpacetimeDB-Only vs Arcane+SpacetimeDB

Under the same workload and pass criteria, the SpacetimeDB-only configuration tops out at 1,000 players (latency-bound). With Arcane+SpacetimeDB and multiple clusters, we observe higher ceilings (e.g. 3,000–5,500 depending on cluster count and persist batching). The comparison is not a general “which is better” statement; it is specific to this single-machine, headless, synthetic workload and to the chosen criteria.

### 5.2 Cluster Count and Replication

Adding clusters reduces the number of entities per cluster but increases the number of replication partners per cluster (*N*−1). On a single machine, total replication traffic grows with *N*. We observed that 3 clusters can sustain 5,000 players (no cap) while 5 clusters at 5,000 failed; 10 clusters at 5,000 and 5,500 passed. So the ceiling is not monotonic in *N*: it reflects the tradeoff between lower per-cluster load and higher replication cost.

### 5.3 Persist Batch Size

Capping the number of entities per SpacetimeDB HTTP request (e.g. 500) was intended to better utilize SpacetimeDB’s request throughput. In practice, multiple sequential requests per persist window extended the time the tick loop spent in persist and increased client-visible errors. For this workload, a single large request per second performed better. We do not generalize beyond this workload and environment.

---

## 6. Reproducibility

Full step-by-step instructions are in **[REPRODUCIBILITY.md](REPRODUCIBILITY.md)**. Summary:

1. **Prerequisites:** Rust, Redis (`redis://127.0.0.1:6379`), SpacetimeDB CLI, PowerShell, Docker (for v2). Start Redis and run `spacetime start` in a separate terminal for legacy swarm scripts.
2. **Clone:** For **v2 without submodules:** `git clone https://github.com/martinjms/arcane-scaling-benchmarks.git`. For **legacy sweeps from source:** `git clone --recurse-submodules …` (or `git submodule update --init --recursive`).
3. **SpacetimeDB-only ceiling:** From repo root,  
   `.\scripts\swarm\Run-SpacetimeDBCeilingSweep.ps1 -FindCeiling -Step 250 -MaxPlayers 2000`  
   (optionally `-NoPublish` if the module is already published).
4. **Arcane+SpacetimeDB scaling:**  
   `.\scripts\swarm\Run-ArcaneScalingSweep.ps1 -NumServers 2 -PlayersTotal 1000`  
   Default is no persist batch cap (`-PersistBatchSize 0`); use that for the reported ceilings.
5. **Outputs:** CSV and logs under `scripts/swarm/` (e.g. `arcane_scaling_sweep.csv`, `spacetimedb_ceiling_sweep.csv`, `arcane_scaling_logs/`). Canonical parameters are in [docs/CANONICAL_PARAMETERS.md](docs/CANONICAL_PARAMETERS.md).
6. **Benchmark v2 (Docker, public reproducibility):** set `ARCANE_INFRA_IMAGE` / `ARCANE_SWARM_IMAGE` to published images, then `.\scripts\benchmark\Run-Benchmark-V2.ps1 -UsePublishedImages`. AWS: [docs/CLOUD_BENCHMARK_AWS.md](docs/CLOUD_BENCHMARK_AWS.md).

---

## 7. Summary of Ceilings (No Cap, Single Machine)

| Configuration              | Ceiling (players) |
|----------------------------|--------------------|
| SpacetimeDB only           | 1,000              |
| Arcane+SpacetimeDB, 1 cluster  | ≥1,750         |
| Arcane+SpacetimeDB, 2 clusters | 3,000           |
| Arcane+SpacetimeDB, 3 clusters | 5,000           |
| Arcane+SpacetimeDB, 4 clusters | 4,000           |
| Arcane+SpacetimeDB, 5 clusters | 4,000           |
| Arcane+SpacetimeDB, 10 clusters | 5,500          |

All runs: 10 Hz tick, 2 aps, 30 s, spread, everyone-sees-everyone. Pass: err_rate &lt; 1%, lat_avg_ms &lt; 200.


## Benchmark v2 (containerized)

A containerized, resource-limited profile is available as an intermediate step toward multi-host deployment.

Run:

`powershell
cd scripts\\benchmark
.\\Run-Benchmark-V2.ps1
` 

See docs/BENCHMARK_V2_METHOD.md for topology, limits, and caveats.

## Benchmark v2 in AWS (one command)

You can run the same v2 benchmark on an ephemeral AWS EC2 host:

`powershell
cd scripts\\cloud
.\\Run-Benchmark-V2-Aws.ps1 -ArtifactBucket <your-s3-bucket> -Region us-east-1
`

This cloud runner provisions a temporary instance, executes v2 remotely, downloads artifacts locally, and cleans up cloud resources by default.

Details: [docs/CLOUD_BENCHMARK_AWS.md](docs/CLOUD_BENCHMARK_AWS.md).

**Artifact bucket (Terraform, recommended):** [infra/aws/terraform/README.md](infra/aws/terraform/README.md).

