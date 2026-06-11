# arcane_swarm

Generic headless swarm runtime.

This repository contains only swarm-specific code and packaging.

## Workspace

- `crates/arcane-swarm` - load-driver binary crate (`arcane-swarm`): simulates game clients (WebSocket + SpacetimeDB SDK) against an Arcane or SpacetimeDB backend.
- `crates/arcane-swarm-orchestrator` - orchestrator binary crate (`arcane-swarm-orchestrator`): central control plane for multi-driver runs — drivers register over a typed WebSocket protocol, receive phase commands, and stream telemetry back.

## Modes

- **Standalone**: run a single `arcane-swarm` driver directly with CLI flags — quick local load tests.
- **Orchestrated**: drivers start with `--orchestrator-url` and zero players, then wait for commands; the orchestrator (driven by the benchmark controller in [arcane-scaling-benchmarks](https://github.com/brainy-bots/arcane-scaling-benchmarks)) sets player counts per phase. This is the mode every published benchmark uses.

## Build

```bash
cargo build --release
```

## Run (examples)

```bash
cargo run -p arcane-swarm --bin arcane-swarm -- --help
cargo run -p arcane-swarm-orchestrator --bin arcane-swarm-orchestrator -- --help
```

## Architecture docs

- [`docs/MODULE_INTERACTIONS.md`](docs/MODULE_INTERACTIONS.md) - crate/module responsibilities and Mermaid interaction graph.
- [`docs/ENGINE_API_BOUNDARY.md`](docs/ENGINE_API_BOUNDARY.md) - reusable engine-facing API boundary for embedding/control tooling.
- [`docs/SWARM_ORCHESTRATOR_DESIGN.md`](docs/SWARM_ORCHESTRATOR_DESIGN.md) - orchestrator control-plane design: protocol, registration, telemetry.
- Library orchestration behavior is validated with mocked-backend tests in `crates/arcane-swarm/src/orchestration.rs`.

## License

arcane_swarm is licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [LICENSE](LICENSE) for the full text.

If you want to ship proprietary/closed-source software that links this code, contact the copyright holder for a commercial license. The AGPL obligations do not apply under a commercial agreement.

For licensing inquiries: martin.mba@gmail.com
