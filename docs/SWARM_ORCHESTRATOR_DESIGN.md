# Swarm Orchestrator — MVP design

**Status:** design discussion (not yet implemented)
**Owner:** Martin
**Date:** 2026-04-28

## Why this exists

Today's benchmark architecture grew organically:

- A PowerShell harness running on the operator's laptop drives the run.
- The harness fans out SSM `RunCommand` calls in parallel to N driver EC2 instances.
- Each driver runs `arcane-swarm` in Docker; it executes a tier ramp and writes per-tier `FINAL` lines to stderr.
- The harness pulls per-driver stderr from S3 *after* all drivers exit; aggregates; declares a winner.

This works, but it leaves three structural problems on the table that get worse as the benchmark suite expands:

1. **No real-time visibility.** A 25-minute run is a black box from the operator's terminal. The "is anything happening?" question can only be answered post-hoc by reading SSM output and S3 results.
2. **No real-time validity gating.** Per-tier validity is computed *after* the run finishes. If tier 3 of 12 fails the latency gate, the harness still runs tiers 4–12, wasting ~20 minutes and ~$4 of fleet time.
3. **No coordination plane between drivers.** Adversarial-motion experiments, cohort bursts, dynamic load shape, and any other "all drivers do X at the same instant" feature is currently emulated by per-driver clock sync, which is fragile. There's no central authority that says "all drivers, switch to motion mode B at T+30s."

The next round of benchmark work — `bytes_out` / DR-hit-rate instrumentation (#94/#95), live dashboard (#97), 24-driver engine-ceiling probe (#98), adversarial-motion benchmark (#99), 24-hour soak (#100) — is dramatically easier with these three problems solved than with another patch on the PowerShell-and-SSM substrate.

This document specifies the minimum viable orchestrator that solves them.

## Adjacent product framing

This is not benchmark scaffolding. A swarm orchestrator with driver registration, real-time aggregation, validity gates, and a dashboard is exactly what a game studio needs to load-test their own production Arcane deployment. The benchmark is the first consumer; "drop in our load-testing rig, point it at your fleet" is a real adjacent product on the same engineering. Treat the orchestrator as a product feature, not as benchmark infra debt.

## High-level architecture

```
┌────────────────────┐        ┌──────────────────────────┐         ┌──────────────────────┐
│ Operator's laptop  │        │  Orchestrator EC2        │         │  Cluster fleet       │
│                    │        │  (in same VPC)           │         │  (4 × c6in.2xlarge)  │
│  orchestrator-cli  │  HTTPS │                          │  HTTP   │                      │
│  (terminal UI)     │ ◄────► │  - driver pool           │ ◄────► │  /stats endpoints    │
│                    │  + SSE │  - tier runner           │         │                      │
└────────────────────┘        │  - stats collector       │         └──────────────────────┘
                              │  - validity gate         │
                              │  - dashboard SSE source  │         ┌──────────────────────┐
                              │  - results writer        │  TCP    │  Driver fleet        │
                              │                          │  (gRPC  │  (12+ × c6in.4xlarge)│
                              │                          │   or    │                      │
                              │                          │   WS)   │  arcane-swarm        │
                              │                          │ ◄────► │  (with registration) │
                              └──────────────────────────┘         └──────────────────────┘
```

**Key invariants:**

- Orchestrator runs in the same VPC as the cluster fleet → direct HTTP `/stats` polling, no SSM round-trips.
- Drivers register with the orchestrator on startup; orchestrator knows the live fleet at all times.
- Operator's laptop talks only to the orchestrator. The PowerShell harness shrinks to "read Terraform output, launch `orchestrator-cli`."

## MVP component list

Each component below is one PR. Each PR ships with a pre-written test file that is the agent's success criterion.

### 1. Driver registration & heartbeat protocol

- New gRPC or WebSocket-over-TLS protocol: drivers `Register(driver_id, capabilities)` on startup, `Heartbeat()` every 5s, `Deregister()` on graceful shutdown.
- Orchestrator maintains a `DriverPool` of healthy drivers (`Active`, `Stale`, `Failed` states; `Stale` after 3 missed heartbeats).
- Driver gains a `--orchestrator-url` flag; if absent, falls back to current standalone mode for backward compatibility.

**Acceptance tests:**
- Registration round-trip succeeds; pool size grows.
- Heartbeat keeps driver in `Active`; missed heartbeats transition to `Stale`.
- Graceful deregister removes driver immediately.
- Pool cap enforced (orchestrator rejects registration past `--max-drivers`).

### 2. Tier ramp coordinator

- Orchestrator owns the test plan (a sequence of tiers: `[(player_count, hold_seconds), ...]`).
- Sends `SetPlayers(N)` / `StartTier(...)` / `EndTier()` commands to all `Active` drivers in parallel.
- Tracks per-tier start/end timestamps and which drivers participated.

**Acceptance tests:**
- 12-driver mock fleet (in-process test): all receive `SetPlayers(125)` simultaneously (within 100ms wall-clock).
- Tier ramp progresses through the configured sequence.
- A driver going `Stale` mid-tier marks the tier `INVALID` and the run aborts.
- Manual abort signal stops the current tier cleanly.

### 3. Cluster `/stats` collector

- Background tokio task; for each cluster URL in config, polls `/stats` every 2 seconds.
- Maintains a rolling 5-minute time series in memory of every counter (`bytes_in`, `bytes_out`, `last_tick_us`, `broadcast_lagged_events`, `entities_current`, plus the new counters from #94 once they land).
- Computes derived rates: `bytes_out_per_sec`, `delta_hit_rate`, `egress_aggregate_gbps`.

**Acceptance tests:**
- Mock cluster server emits known stats; collector reports correct rates and counters.
- Polling continues when one cluster is briefly unreachable; resumes on recovery.
- Time-series memory bounded to 5 minutes (no unbounded growth over a 24-hour soak).

### 4. Real-time validity gate

- Per-tier acceptance criteria: error rate, latency, entity count, broadcast cadence, etc. Configurable.
- Re-evaluated every 5 seconds during a tier; if a gate fails for ≥3 consecutive evaluations during steady-state, the tier is marked `FAIL` and the ramp aborts.

**Acceptance tests:**
- Synthetic tier with all gates passing → tier marked `PASS`.
- Synthetic tier with latency exceeding gate for 3 consecutive evaluations → tier marked `FAIL`, ramp aborts within 15s of the breach.
- Tier marked `FAIL` short-circuits subsequent tier execution.

### 5. Dashboard SSE source + terminal CLI

- Orchestrator exposes `/dashboard/stream` as Server-Sent Events.
- Each event is a JSON snapshot of: current tier, elapsed time, per-driver status, per-driver lat (when available), per-cluster `/stats` summary, validity gate status, tier history.
- `orchestrator-cli` connects via SSE and renders the 13-panel layout (per #97) using ANSI cursor codes.

**Acceptance tests:**
- SSE stream emits a valid JSON event every ≤2s during a run.
- CLI connects, renders, reconnects automatically on transient network drop.
- Stream completes cleanly when run finishes.

### 6. Results writer

- At the end of every tier, orchestrator writes `tier_<N>.json` (full snapshot of all per-driver and per-cluster data).
- At end of run, writes `manifest.json` (run summary, headline numbers, validity outcomes).
- Writes both locally on the orchestrator EC2 and to S3 (artifact bucket from current Terraform).

**Acceptance tests:**
- After a 3-tier mock run, `tier_1.json` / `tier_2.json` / `tier_3.json` / `manifest.json` exist with correct schema.
- S3 upload succeeds; matches local content.
- Resumable: orchestrator restart picks up where it left off without losing earlier tier data.

## Driver protocol extension

A small change to `arcane-swarm` (the existing driver binary):

- New CLI flag: `--orchestrator-url <url>`. When present, switches from standalone mode (current) to orchestrated mode (new).
- New tokio task that connects to orchestrator, registers, and listens for control messages.
- Existing per-tier metric reporting becomes `tier_progress(driver_id, tier, mean_lat, errors, players)` push events to orchestrator instead of stderr `FINAL` lines.

Standalone mode (no `--orchestrator-url`) keeps the current behavior intact. This preserves the existing benchmark path during the transition and lets local development continue without an orchestrator running.

**Acceptance tests:**
- Run driver in standalone mode: identical output to today's benchmark.
- Run driver in orchestrated mode against a mock orchestrator: registration succeeds, control messages received, tier progress pushed.

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

This pattern is being established with the Rapier physics work (PR 1 lays test scaffolding, agent's PR 2 implements until tests pass) and applies identically to every orchestrator component.

## What's deliberately out of scope for MVP

- **Browser-based dashboard.** Terminal CLI is enough. Browser version can come later if a customer asks.
- **Mid-run dynamic driver scale-up.** Fleet is fixed at run start.
- **Multi-region orchestration.** Single-region only. Multi-region is its own track (#101).
- **Driver provisioning.** Stays Terraform. Orchestrator only talks to running drivers.
- **Replacement of the existing PowerShell harness for non-orchestrated paths.** Standalone driver mode keeps the existing path alive.
- **Authentication beyond mTLS within the VPC.** Operator-laptop-to-orchestrator is plain HTTPS over a known instance ID; security group keeps it scoped to the operator's CIDR.

## Phased delivery

1. **Foundation PR (this design + test scaffolding).** This document + a `crates/arcane-swarm-orchestrator/` skeleton + the test files for components 1–6, all tests `#[ignore]`-d. Lands as one PR; no behavior change. ~1 day of work.
2. **Component PRs 1–6**, sequenced: registration → tier runner → stats collector → validity gate → dashboard → results writer. Each unignores its own tests; each is one agent task. ~1–2 weeks total.
3. **Driver protocol extension PR.** Adds `--orchestrator-url` to existing `arcane-swarm`. ~1 day. Can be parallel to component PRs since the test fixtures use a mock orchestrator.
4. **PowerShell harness shrink.** Existing `Run-Benchmark-Aws.ps1` becomes ~50 lines that reads Terraform output and launches `orchestrator-cli`. Old code paths preserved as legacy fallback. ~1 day.
5. **Re-run the headline benchmark on the orchestrator** to validate end-to-end parity. Same 13,500-CCU result, now with real-time dashboard and per-tier instrumentation.

## Open decisions

- **Wire protocol for driver ↔ orchestrator: gRPC or WebSocket?** gRPC is more "right" for an RPC system but adds proto compilation; WebSocket-over-TLS is simpler and well-supported in tokio. Recommend **WebSocket for MVP** (we already do WebSocket between driver and cluster); revisit if/when we want streaming + flow control.
- **State persistence on orchestrator restart.** MVP: in-memory only; restart aborts the run cleanly. Post-MVP: optional SQLite checkpoint for soak-test resilience.
- **Authentication.** MVP: assume the operator owns the VPC (security group restricts to operator CIDR). Post-MVP: mTLS or JWT-bearer if there's ever a multi-tenant scenario.

## Definition of done for the MVP

The orchestrator is "MVP done" when all six of these pass:

1. The current 13,500-CCU headline benchmark completes via the orchestrator with results within 1% of the baseline.
2. Real-time dashboard renders all 13 panels (per #97) during the run.
3. Per-tier validity gate aborts a synthetic-failure run within 15s of breach.
4. All acceptance tests across all six components pass in CI.
5. PowerShell harness has been shrunk to operator-cli launcher.
6. README's "Reproduce in 10 minutes" still works end-to-end (now via orchestrator-cli instead of the old harness).
