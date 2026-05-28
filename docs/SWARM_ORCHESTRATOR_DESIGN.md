# Swarm Orchestrator — design

**Status:** design discussion (not yet implemented)
**Owner:** Martin
**Date:** 2026-04-28 (revised 2026-05-02)

## Why this exists

The benchmark harness grew organically (and still contains legacy scripts kept for emergencies):

- A PowerShell harness running on the operator's laptop drives *some* paths.
- **Historical / internal-only path:** the harness fans out SSM `RunCommand` calls in parallel to N driver EC2 instances.
- Each driver runs `arcane-swarm` in Docker; it executes a tier ramp and writes per-tier `FINAL` lines to stderr.
- In that historical path, the harness pulls per-driver stderr from S3 *after* all drivers exit; aggregates; declares a winner.

**Supported public reproduction today:** start the fleet + orchestrator, then run local `benchmark-controller` against the orchestrator HTTP API (`Run-Benchmark-Aws-Controller.ps1`), which renders the real-time terminal dashboard by subscribing to orchestrator SSE. This design doc treats that controller/orchestrator split as the direction of travel even where legacy harness pieces still exist in-repo.

This works, but it leaves three structural problems on the table that get worse as the benchmark suite expands:

1. **No real-time visibility.** A 25-minute run is a black box from the operator's terminal.
2. **No real-time control.** Per-tier validity is computed *after* the run finishes. If tier 3 of 12 fails the latency gate, the harness still runs tiers 4–12, wasting ~20 minutes and ~$4 of fleet time.
3. **No coordination plane between drivers.** Adversarial-motion experiments, cohort bursts, dynamic load shape, and any other "all drivers do X at the same instant" feature is currently emulated by per-driver clock sync, which is fragile.

This document specifies the orchestrator that solves them — and the architectural separation that keeps it general-purpose.

## Architectural separation: orchestrator vs controller

This is the load-bearing decision in this design.

**The orchestrator is a real-time instruction dispatcher.** It owns:

- Driver pool (registration, heartbeat, state tracking)
- Real-time command broadcast (relays commands like `SetPlayers(N)` to all active drivers)
- Cluster `/stats` collection (background polling)
- Live telemetry stream (SSE) — fleet state, command log, cluster stats
- Telemetry archive (periodic snapshots for operator reference)

**The orchestrator does NOT own:**

- The test plan or "phases"
- When to ramp player count
- Burst scheduling and per-phase weighting
- Validity gates / pass-fail evaluation
- Tier sequencing / tier-level results

**That logic lives in a *benchmark controller* — a separate crate in the `arcane-scaling-benchmarks` repo.** The controller:

- Knows the test plan (phases, durations, ramp shape)
- Sends real-time commands to the orchestrator on schedule
- Subscribes to the orchestrator's SSE telemetry stream
- Evaluates per-phase validity gates against telemetry
- Writes per-phase / per-tier results

**Why this split matters.** The orchestrator becomes a general-purpose load-driver that *anything* can drive in real time: the benchmark, a soak test, an ad-hoc operator session, a future product. The benchmark controller is the domain logic that the benchmark project owns. Adversarial-motion experiments, dynamic load shape, soak runs — each is a different controller talking to the same orchestrator. The orchestrator carries no benchmark vocabulary; the controller carries no driver-coordination plumbing.

**The driver also stays oblivious to phases.** It receives `SetPlayers(N)` and `SetSpawnDelayMs(ms)`, mutates its local fleet slice, emits telemetry. It does not know about tier sequences or test plans. This is the most likely place for the old "phase logic in the orchestrator" architecture to silently leak back in — it must not.

## Adjacent product framing

This is not benchmark scaffolding. A swarm orchestrator with driver registration, real-time aggregation, and a telemetry stream is exactly what a game studio needs to load-test their own production Arcane deployment. The benchmark is the first consumer; "drop in our load-test rig, point it at your fleet, drive it from your own controller" is a real adjacent product on the same engineering. The clean orchestrator/controller split makes this real — anyone can write a controller; the orchestrator stays domain-neutral.

## High-level architecture

```
┌──────────────────────┐       ┌────────────────────────────┐       ┌────────────────────┐
│  Benchmark           │       │  Orchestrator EC2          │  HTTP │  Cluster fleet     │
│  controller          │  WS   │  (in same VPC)             │ ◄────►│  (4 × c6in.2xlarge)│
│  (in benchmarks repo)│ ◄────►│                            │       │  /stats endpoints  │
│                      │       │  - driver pool             │       │                    │
│  - phase logic       │       │  - command dispatch        │       └────────────────────┘
│  - ramp schedule     │  SSE  │  - stats collector         │
│  - burst scheduling  │ ◄─────│  - telemetry SSE source    │       ┌────────────────────┐
│  - validity gates    │       │  - telemetry archive       │  WS   │  Driver fleet      │
│  - per-phase results │       │                            │ ◄────►│  (12+ drivers)     │
└──────────────────────┘       │                            │       │  arcane-swarm      │
                               │                            │       │  (orchestrated     │
┌──────────────────────┐  SSE  │                            │       │   mode)            │
│  Operator laptop     │ ◄─────│                            │       └────────────────────┘
│  orchestrator-cli    │       │                            │
│  (live dashboard)    │       │                            │
└──────────────────────┘       └────────────────────────────┘
```

**Key invariants:**

- Orchestrator runs in the same VPC as the cluster fleet → direct HTTP `/stats` polling, no SSM round-trips.
- Drivers register with the orchestrator on startup; orchestrator knows the live fleet at all times.
- Multiple SSE subscribers can connect at once (benchmark controller + operator dashboard + future tools).
- The orchestrator command vocabulary is intentionally minimal and domain-neutral — `SetPlayers`, `SetSpawnDelayMs`, `Stop`. New commands are added as the controller needs them; the orchestrator never gains "tier" or "phase" verbs.

## Component list

Each component below is one PR. Each PR ships with a pre-written test file that is the agent's success criterion.

### 1. Driver registration & heartbeat protocol — ✅ done (PR #34, PR #36)

- WebSocket-over-TLS protocol: drivers `Register(driver_id, capabilities)` on startup, `Heartbeat()` every 5s, `Deregister()` on graceful shutdown.
- Orchestrator maintains a `DriverPool` of healthy drivers (`Active`, `Stale`, `Failed` states; `Stale` after 3 missed heartbeats).
- Driver gains a `--orchestrator-url` flag; if absent, falls back to current standalone mode.

**All acceptance tests pass in CI.**

### 2. Real-time command dispatch (replaces the old "tier ramp coordinator")

- Orchestrator exposes a command-submission endpoint on the same WebSocket plane controllers connect to.
- Authorized clients submit commands; orchestrator broadcasts them to all `Active` drivers in parallel.
- Per-driver delivery acknowledgment is recorded; the command + acks land in the telemetry archive.
- **Initial command set** (minimum to recreate today's benchmark behavior):
  - `SetPlayers(N)` — total fleet target; drivers split N proportionally across themselves
  - `SetSpawnDelayMs(ms)` — join-rate pacing during scale-up (mirrors the existing `--inter-spawn-delay-ms` flag from PR #17)
  - `Stop` — clean shutdown; drivers tear down their fleet slices and deregister
- The command set is intentionally extensible. New commands (motion modes, interaction bursts, weighting) get added when a controller needs them. The orchestrator never gains benchmark-specific verbs.

**Acceptance tests:**
- 12-driver mock fleet receives a `SetPlayers(125)` broadcast within 100 ms wall-clock (all drivers acknowledge inside the window).
- Per-driver delivery acknowledgment is recorded; orchestrator surfaces which drivers acked and which did not.
- A driver going `Stale` mid-broadcast is logged but does not block delivery to the rest of the fleet.
- Unknown command type → orchestrator returns a typed error to the submitter; no broadcast attempted.
- Multiple controllers can submit commands concurrently; each command + ack pair is logged with submitter identity.

### 3. Cluster `/stats` collector

- Background tokio task; for each cluster URL in config, polls `/stats` every 2 seconds.
- Maintains a rolling 5-minute time series in memory of every counter (`bytes_in`, `bytes_out`, `last_tick_us`, `broadcast_lagged_events`, `entities_current`, plus the new counters as they land).
- Computes derived rates: `bytes_out_per_sec`, `delta_hit_rate`, `egress_aggregate_gbps`.

**Acceptance tests:**
- Mock cluster server emits known stats; collector reports correct rates and counters.
- Polling continues when one cluster is briefly unreachable; resumes on recovery.
- Time-series memory bounded to 5 minutes (no unbounded growth over a 24-hour soak).

### 4. ~~Real-time validity gate~~ — moved to benchmark controller

The original component 4 (real-time validity gate) is **out of scope for the orchestrator.** Validity gating is benchmark domain logic; the orchestrator surfaces telemetry, the controller in the benchmark repo evaluates gates against it. Tracked separately in `arcane-scaling-benchmarks`.

### 5. Telemetry SSE source + dashboard CLI

- Orchestrator exposes `/telemetry/stream` as Server-Sent Events.
- Each event is a JSON snapshot of: fleet state (per-driver status), recent command log, per-cluster `/stats` summary.
- `orchestrator-cli` connects via SSE and renders a live terminal dashboard.
- The benchmark controller subscribes to the same stream to evaluate validity gates.

**Acceptance tests:**
- SSE stream emits a valid JSON event every ≤2s during a run.
- CLI connects, renders, reconnects automatically on transient network drop.
- Multiple subscribers (CLI + a synthetic controller) both receive every event.

### 6. Telemetry archive (replaces "results writer")

- Orchestrator periodically writes a snapshot of telemetry + command log to local disk and S3 (artifact bucket from current Terraform).
- This is operator-facing operational data — *not* benchmark "results". Per-phase / per-tier result writing is the controller's job.

**Acceptance tests:**
- After a 60s mock run, snapshots exist locally and in S3.
- Snapshot schema includes: timestamp, command-log slice, fleet state, per-cluster `/stats` summaries.
- Resumable: orchestrator restart picks up where it left off without overwriting earlier snapshots.

## Driver protocol extension

A small change to `arcane-swarm` (the existing driver binary):

- New CLI flag: `--orchestrator-url <url>`. When present, switches from standalone mode (current) to orchestrated mode.
- New tokio task that connects to orchestrator, registers, and listens for commands.
- **The driver knows nothing about tiers, phases, or test plans.** It receives `SetPlayers(N)` and `SetSpawnDelayMs(ms)`, mutates its local fleet slice, emits telemetry events.
- Per-driver telemetry is pushed to the orchestrator as events (replaces stderr `FINAL` lines that standalone mode still emits).

Standalone mode (no `--orchestrator-url`) keeps the current behavior intact. This preserves the existing benchmark path during the transition and lets local development continue without an orchestrator running.

**Acceptance tests:**
- Standalone mode: identical output to today's benchmark.
- Orchestrated mode: registration succeeds, commands received, telemetry pushed.
- Driver receiving `SetPlayers(N)` adjusts its fleet slice; receiving `SetSpawnDelayMs(ms)` adjusts its internal pacer.

## Test-driven agent supervision pattern

This is the canonical pattern for every component above and every future feature:

1. **Tests written first**, by a human, into the repo on the foundation PR.
2. **Tests added to CI** so they run on every push.
3. **Tests are the spec.** No prose ambiguity about what "done" means.
4. **Agent task = "make these tests pass."** No architectural decisions delegated to the agent.
5. **PR for human review before merge.** Agent never merges.

For each component PR, the structure is:
- Foundation PR (human): trait/interface definitions + test file with all acceptance tests scaffolded (or `#[ignore]`-d if they require an unfinished implementation).
- Implementation PR (agent): write the implementation that makes the tests pass.

## What's deliberately out of scope

- **Browser-based dashboard.** Terminal CLI is enough; browser version added when there is demand.
- **Mid-run dynamic driver scale-up.** Fleet is fixed at run start; scale-up requires its own design.
- **Multi-region orchestration.** Single-region only; multi-region requires its own design.
- **Driver provisioning.** Stays Terraform.
- **Replacement of the legacy PowerShell harness for non-orchestrated paths.** Standalone driver mode keeps an internal-only harness alive during the controller rollout (not advertised as the public reproduction path).
- **Authentication beyond VPC scoping.** Operator-laptop-to-orchestrator is plain HTTPS over a known instance ID; security group keeps it scoped to the operator's CIDR. Stronger auth (mTLS / JWT) is added when the deployment requires it.

## Delivery sequence

1. **Foundation PR** (this design + test scaffolding). Largely done — C1 + skeleton merged. Remaining scaffolding for command dispatch, stats collector, SSE, telemetry archive lands on the existing branch.
2. **Component PRs**: command dispatch (C2) → stats collector (C3) → telemetry SSE (C5) → telemetry archive (C6). Each unignores its own tests; each is one agent task.
3. **Driver protocol extension PR.** Adds `--orchestrator-url` and orchestrated-mode task to existing `arcane-swarm`.
4. **Benchmark controller** (new crate in `arcane-scaling-benchmarks`): owns phase logic, drives the orchestrator. Tracked as a separate epic in that repo.
5. **Operator launcher.** `Run-Benchmark-Aws-Controller.ps1` is the supported entrypoint: read Terraform `benchmark_state`, start the AWS-side containers, then run local `benchmark-controller` against the orchestrator HTTP API (terminal dashboard via controller SSE subscription).
6. **Re-run the headline benchmark** end-to-end via controller → orchestrator → drivers; validate parity with prior baseline within 1%.

## Open decisions

- **Wire protocol for driver ↔ orchestrator and controller ↔ orchestrator: WebSocket-over-TLS** (recommended). Already used between driver and cluster.
- **State persistence on orchestrator restart.** Default: in-memory only; a restart aborts the run cleanly. Optional SQLite checkpoint can be added when soak-test resilience requires it.
- **Authentication.** Default: VPC-scoped HTTPS only. mTLS or JWT-bearer added when a multi-tenant deployment requires it.

## Definition of done

The orchestrator is done when all six of these pass:

1. The current 13,500-CCU headline benchmark completes via [benchmark controller → orchestrator → drivers] with results within 1% of the prior baseline.
2. Real-time terminal dashboard renders during the run (subscribed to orchestrator SSE).
3. Per-phase validity gate **in the benchmark controller** aborts a synthetic-failure run within 15s of breach.
4. All acceptance tests across orchestrator components pass in CI.
5. PowerShell harness shrunk to operator-cli launcher.
6. The benchmark repo README's public reproduction path works end-to-end (controller → orchestrator → drivers).
