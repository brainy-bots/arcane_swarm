# Engine API Boundary

This document defines the reusable engine-facing API boundary for `arcane-swarm`.

## Purpose

The CLI binary remains the default entrypoint, but embedding tools should depend on library exports instead of binary-internal modules.

## Stable boundary

- `arcane_swarm::EngineRunConfig`
- `arcane_swarm::EngineSummary`
- `arcane_swarm::EngineRunHandle` (trait)
- `arcane_swarm::SwarmEngine` (trait)

These are defined in `crates/arcane-swarm/src/engine_api.rs`.

## Non-boundary internals

The following remain implementation details and can change freely:

- `src/bin/arcane_swarm/main.rs`
- `src/bin/arcane_swarm/spawn_context.rs`
- `src/bin/arcane_swarm/backends_*.rs`

## Migration guideline

If an external script/tool needs runtime control (`set players`, `request summary`, `stop`), build against the `engine_api` traits and keep binary process wiring behind an adapter.
