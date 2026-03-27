# arcane_swarm

Generic headless swarm runtime.

This repository contains only swarm-specific code and packaging.

## Workspace

- `crates/arcane-swarm` - swarm binary crate (`arcane-swarm`)

## Build

```bash
cargo build -p arcane-swarm --bin arcane-swarm --release
```

## Run (example)

```bash
cargo run -p arcane-swarm --bin arcane-swarm -- --help
```

## Architecture docs

- [`docs/MODULE_INTERACTIONS.md`](docs/MODULE_INTERACTIONS.md) - crate/module responsibilities and Mermaid interaction graph.
- [`docs/ENGINE_API_BOUNDARY.md`](docs/ENGINE_API_BOUNDARY.md) - reusable engine-facing API boundary for embedding/control tooling.
- Library orchestration behavior is validated with mocked-backend tests in `crates/arcane-swarm/src/orchestration.rs`.
