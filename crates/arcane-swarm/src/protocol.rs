//! Wire-format helpers shared by backends (e.g. Arcane WebSocket payloads).

/// Spatial query radius (server units) for SpacetimeDB read simulation.
pub const VISIBILITY_RADIUS: f64 = 1500.0;

/// JSON for Arcane cluster `PLAYER_STATE` WebSocket messages.
pub fn player_state_json(
    id: &uuid::Uuid,
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
) -> String {
    format!(
        r#"{{"type":"PLAYER_STATE","entity_id":"{}","position":{{"x":{},"y":{},"z":{}}},"velocity":{{"x":{},"y":{},"z":{}}}}}"#,
        id, x, y, z, vx, vy, vz
    )
}
