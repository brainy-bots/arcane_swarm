# Reproducing the experiment

This document gives step-by-step instructions to run the same scaling experiments on your machine. The repository is self-contained: it includes the orchestration scripts and vendors the SpacetimeDB module source and swarm runtime code.

The only Git submodule needed is **`arcane/`** (for the Arcane manager/cluster binaries).

---

## Prerequisites

- **Rust** (stable): [rustup.rs](https://rustup.rs)
- **Redis**: running locally, default `redis://127.0.0.1:6379`
- **SpacetimeDB**: [Install the CLI](https://spacetimedb.com/docs), then run `spacetime start` in a separate terminal (must be running before the scripts publish the module and during runs).
- **PowerShell**: scripts are written for Windows PowerShell.

---

## 1. Clone the repository and submodules

```powershell
git clone --recurse-submodules https://github.com/martinjms/arcane-scaling-benchmarks.git
cd arcane-scaling-benchmarks
```

If you already cloned without `--recurse-submodules`:

```powershell
git submodule update --init --recursive
```

You should see `arcane/` populated. The scripts expect it as a sibling directory of the benchmark repo root.

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
