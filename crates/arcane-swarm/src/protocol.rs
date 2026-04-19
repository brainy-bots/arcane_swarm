//! Wire-format helpers shared by backends (e.g. Arcane WebSocket payloads).
//!
//! The Arcane wire protocol uses postcard-encoded binary frames from
//! [`arcane_wire`]. This module exposes small helpers that build the
//! binary frame bytes from the values a backend loop already has on hand,
//! so backend code doesn't need to know about the wire schema.
//!
//! Binary (not JSON) for benchmark fairness with SpacetimeDB's BSATN
//! default — see brainy-bots/arcane#28 for the motivation + microbench.

use arcane_wire::{
    encode_client, ClientFrame, GameActionPayload, PlayerStatePayload, Vec3 as WireVec3,
};

/// Spatial query radius (server units) for SpacetimeDB read simulation.
pub const VISIBILITY_RADIUS: f64 = 1500.0;

/// Encode one `PLAYER_STATE` frame as postcard bytes for the Arcane cluster
/// WebSocket. Returned bytes are ready to send as `Message::Binary`.
pub fn encode_player_state(
    id: &uuid::Uuid,
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
) -> Vec<u8> {
    let frame = ClientFrame::PlayerState(PlayerStatePayload {
        entity_id: *id,
        position: WireVec3::new(x, y, z),
        velocity: WireVec3::new(vx, vy, vz),
        user_data: Vec::new(),
    });
    // encode_client returns Err only on allocator failure or serialize-bug;
    // both are fatal rather than recoverable for a benchmark client — unwrap
    // so any such bug fails loudly instead of silently dropping messages.
    encode_client(&frame).expect("postcard encode of ClientFrame::PlayerState cannot fail")
}

/// Encode one `GAME_ACTION` frame as postcard bytes for the Arcane cluster
/// WebSocket. `payload` is treated as opaque application bytes (typically
/// the caller passes in JSON bytes).
pub fn encode_game_action(entity_id: &uuid::Uuid, action_type: &str, payload: &[u8]) -> Vec<u8> {
    let frame = ClientFrame::Action(GameActionPayload {
        entity_id: *entity_id,
        action_type: action_type.to_string(),
        payload: payload.to_vec(),
    });
    encode_client(&frame).expect("postcard encode of ClientFrame::Action cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcane_wire::decode_client;

    #[test]
    fn encode_player_state_roundtrips() {
        let id = uuid::Uuid::from_u128(0x1111_2222);
        let bytes = encode_player_state(&id, 1.25, 2.5, 3.75, 0.1, 0.0, -0.1);
        let decoded = decode_client(&bytes).unwrap();
        let ClientFrame::PlayerState(payload) = decoded else {
            panic!("expected PlayerState variant");
        };
        assert_eq!(payload.entity_id, id);
        assert_eq!(payload.position.x, 1.25);
        assert_eq!(payload.velocity.z, -0.1);
        assert!(payload.user_data.is_empty());
    }

    #[test]
    fn encode_game_action_roundtrips_with_payload() {
        let id = uuid::Uuid::from_u128(7);
        let json = br#"{"item_type":5}"#;
        let bytes = encode_game_action(&id, "use_item", json);
        let decoded = decode_client(&bytes).unwrap();
        let ClientFrame::Action(payload) = decoded else {
            panic!("expected Action variant");
        };
        assert_eq!(payload.entity_id, id);
        assert_eq!(payload.action_type, "use_item");
        assert_eq!(payload.payload, json);
    }
}
