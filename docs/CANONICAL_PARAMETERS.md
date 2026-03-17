# Canonical benchmark parameters

These parameters are **fixed** across all ceiling/scaling experiments so that SpacetimeDB-only and Arcane+SpacetimeDB results are comparable.

| Parameter | Value | Notes |
|-----------|--------|------|
| **tick_rate_hz** | 10 | Position updates per second per player |
| **aps** | 2 | Actions per second per player |
| **duration_s** | 30 | Run duration (warmup is additional in swarm) |
| **mode** | spread | Movement spread |
| **visibility** | everyone_sees_everyone | All clients receive all entity positions |
| **demo_entities** | 0 | No NPCs; players only |
| **server_physics** | true | (SpacetimeDB-only) Physics in module |
| **read_rate_hz** | 5 | (Arcane+Spacetime) World-state read rate per player |
| **spacetimedb_persist_hz** | 1 | (Arcane+Spacetime) Batch persist to SpacetimeDB per second |
| **spacetimedb_persist_batch_size** | 0 recommended | 0 = one request per persist window; 500 = cap (worse ceilings in our runs) |
| **redis_enabled** | true | (Arcane+Spacetime) Replication when num_servers > 1 |
| **pass_criteria** | err_rate < 1%, lat_avg_ms < 200 | Pass/fail per run |

Scripts print this block at the start of each run and write the same fields into the CSV.
