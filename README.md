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
