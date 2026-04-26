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
    encode_client, ClientFrame, GameActionPayload, PlayerStatePayload, Vec3 as WireVec3, Vec3Q,
};

/// Spatial query radius (server units) for SpacetimeDB read simulation.
pub const VISIBILITY_RADIUS: f64 = 1500.0;

/// Encode one `PLAYER_STATE` frame as postcard bytes for the Arcane cluster
/// WebSocket. Returned bytes are ready to send as `Message::Binary`.
///
/// `user_data` is opaque per-entity payload that flows through the cluster's
/// existing `EntityStateEntry.user_data` path back out to subscribers. Pass an
/// empty slice for the lean baseline; for the realistic-state benchmark the
/// caller fills it via [`fill_pseudo_user_data`] so the wire carries varied
/// (not constant-zero) bytes — important for a fair comparison once
/// per-message-deflate (arcane#44) is enabled.
#[allow(clippy::too_many_arguments)]
pub fn encode_player_state(
    id: &uuid::Uuid,
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
    user_data: &[u8],
) -> Vec<u8> {
    // Quantize at the wire boundary: continuous f64 from the simulated
    // player tick becomes i16 on the wire (~3-9 B per Vec3 vs 24 B). See
    // arcane_wire::Vec3Q for the scale + range tradeoff. Sub-unit precision
    // is lost; the benchmark world's noise floor (collision_radius=50) is
    // well above 1 unit so this is invisible to the kinematic sim.
    let frame = ClientFrame::PlayerState(PlayerStatePayload {
        entity_id: *id,
        position: Vec3Q::from_vec3(WireVec3::new(x, y, z)),
        velocity: Vec3Q::from_vec3(WireVec3::new(vx, vy, vz)),
        user_data: user_data.to_vec(),
    });
    // encode_client returns Err only on allocator failure or serialize-bug;
    // both are fatal rather than recoverable for a benchmark client — unwrap
    // so any such bug fails loudly instead of silently dropping messages.
    encode_client(&frame).expect("postcard encode of ClientFrame::PlayerState cannot fail")
}

/// Fill `buf` with `len` deterministic-but-varied bytes derived from
/// `(player_seed, tick)`. Caller-owned buffer so the swarm's per-tick path
/// reuses a single allocation per player instead of allocating fresh.
///
/// Why deterministic-but-varied:
/// - **Reproducibility**: same `(seed, tick)` ⇒ same bytes. A future PR that
///   adds a regression check for "did UserDataBytes change broadcast wire
///   bytes?" can compare runs apples-to-apples.
/// - **Not constant**: a constant payload (e.g. `vec![0u8; len]`) compresses
///   to near-nothing under per-message-deflate. That would make the realistic
///   measurement accidentally measure "PMD on highly-redundant data" rather
///   than "realistic game payload over the wire". xorshift output has the
///   compression characteristics of typical mixed game state — some structure
///   but not uniformly redundant.
///
/// Uses xorshift64* (Marsaglia, 2003): 1 multiplication + 3 shifts + 3 xors
/// per 8 bytes. Negligible cost vs the encode + send path, and it matches the
/// "cheap PRNG" caveat in arcane-scaling-benchmarks#52.
pub fn fill_pseudo_user_data(buf: &mut Vec<u8>, len: usize, player_seed: u64, tick: u64) {
    buf.clear();
    if len == 0 {
        return;
    }
    buf.reserve_exact(len);
    let mut state =
        player_seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tick.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_BABE;
    }
    while buf.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let out = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let bytes = out.to_le_bytes();
        let take = (len - buf.len()).min(8);
        buf.extend_from_slice(&bytes[..take]);
    }
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
        let bytes = encode_player_state(&id, 1.25, 2.5, 3.75, 0.1, 0.0, -0.1, &[]);
        let decoded = decode_client(&bytes).unwrap();
        let ClientFrame::PlayerState(payload) = decoded else {
            panic!("expected PlayerState variant");
        };
        assert_eq!(payload.entity_id, id);
        // Vec3Q quantization: 1.25 rounds to 1; -0.1 rounds to 0.
        assert_eq!(payload.position.x, 1i16);
        assert_eq!(payload.velocity.z, 0i16);
        assert!(payload.user_data.is_empty());
    }

    #[test]
    fn encode_player_state_carries_user_data() {
        let id = uuid::Uuid::from_u128(7);
        let payload = vec![0xAB, 0xCD, 0xEF, 0x12, 0x34];
        let bytes = encode_player_state(&id, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, &payload);
        let decoded = decode_client(&bytes).unwrap();
        let ClientFrame::PlayerState(p) = decoded else {
            panic!("expected PlayerState variant");
        };
        assert_eq!(p.user_data, payload);
    }

    #[test]
    fn fill_pseudo_user_data_is_deterministic_per_seed_tick() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        fill_pseudo_user_data(&mut a, 100, 42, 1000);
        fill_pseudo_user_data(&mut b, 100, 42, 1000);
        assert_eq!(a, b, "same (seed, tick) must produce identical bytes");
        assert_eq!(a.len(), 100);
    }

    #[test]
    fn fill_pseudo_user_data_varies_across_ticks() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        fill_pseudo_user_data(&mut a, 64, 42, 1000);
        fill_pseudo_user_data(&mut b, 64, 42, 1001);
        assert_ne!(a, b, "consecutive ticks must produce different bytes");
    }

    #[test]
    fn fill_pseudo_user_data_varies_across_players() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        fill_pseudo_user_data(&mut a, 64, 42, 1000);
        fill_pseudo_user_data(&mut b, 64, 43, 1000);
        assert_ne!(a, b, "different player seeds must produce different bytes");
    }

    #[test]
    fn fill_pseudo_user_data_zero_length_is_empty() {
        let mut buf = vec![0xFFu8; 16];
        fill_pseudo_user_data(&mut buf, 0, 1, 1);
        assert!(buf.is_empty(), "len=0 must clear the buffer");
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
