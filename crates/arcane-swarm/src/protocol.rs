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

/// Fill `buf` with approximately `len` bytes of deterministic-but-varied
/// JSON-shaped payload derived from `(player_seed, tick)`. Caller-owned
/// buffer so the swarm's per-tick path reuses a single allocation per
/// player.
///
/// Output shape: `{"d":"<hex>"}` where `<hex>` is the hex encoding of
/// xorshift64* bytes. Total length lands within ±1 byte of `len` for
/// `len >= 10`; smaller `len` clamps up to a 10-byte minimum so the
/// envelope stays a valid JSON object.
///
/// **Why JSON-shaped, not raw bytes.** The cluster's
/// `entry_from_wire_player_state` calls `serde_json::from_slice` on the
/// wire's `user_data` field. Raw entropy bytes aren't valid JSON and
/// every PlayerState was rejected (parse_failure) — discovered during
/// the 2026-04-26 realistic-state run E. The hex envelope is the cheap
/// fix: stays JSON-parseable on the cluster side; the hex content keeps
/// the entropy that was the original goal of this helper (so PMD
/// compression measurements aren't accidentally measuring "compressing
/// constant zeros"); the envelope adds only ~8 bytes of structural
/// overhead.
///
/// **Why hex and not base64.** Hex needs no extra crate dependency, has
/// a fixed 2× expansion factor (so length math is predictable), and uses
/// only characters that are JSON-safe with no escape sequences. base64
/// would be denser but requires either a dep or hand-written encoder for
/// no benchmark-relevant gain.
///
/// Why deterministic-but-varied:
/// - **Reproducibility**: same `(seed, tick)` ⇒ same bytes.
/// - **Not constant**: a constant payload compresses to near-nothing
///   under per-message-deflate (arcane#44 when it lands). That would
///   make the realistic-state measurement accidentally measure "PMD on
///   highly-redundant data" rather than "realistic game payload".
pub fn fill_pseudo_user_data(buf: &mut Vec<u8>, len: usize, player_seed: u64, tick: u64) {
    buf.clear();
    if len == 0 {
        return;
    }
    // Envelope `{"d":""}` is 8 bytes; hex content is 2 bytes per raw byte.
    // Smallest valid envelope is `{"d":"00"}` = 10 bytes — clamp `len` up.
    let target = len.max(10);
    let raw_len = (target - 8) / 2;
    let mut raw: Vec<u8> = Vec::with_capacity(raw_len.max(1));
    let mut state =
        player_seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tick.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_BABE;
    }
    while raw.len() < raw_len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let out = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let bytes = out.to_le_bytes();
        let take = (raw_len - raw.len()).min(8);
        raw.extend_from_slice(&bytes[..take]);
    }
    let mut hex = String::with_capacity(raw.len() * 2);
    for b in &raw {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", b);
    }
    // serde_json::to_vec emits compact JSON (no whitespace) so the byte
    // length matches the envelope-overhead-plus-hex math above.
    let mut payload = serde_json::Map::new();
    payload.insert("d".to_string(), serde_json::Value::String(hex));
    let bytes = serde_json::to_vec(&serde_json::Value::Object(payload))
        .expect("serializing a tiny JSON map cannot fail");
    buf.extend_from_slice(&bytes);
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
        // Length: 8-byte envelope + 2 × raw_len hex chars; raw_len = (100-8)/2 = 46.
        // Total = 8 + 92 = 100. The math is exact for even (len-8).
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
    fn fill_pseudo_user_data_output_is_valid_json() {
        // The whole point of the JSON-envelope rewrite: the cluster's
        // serde_json::from_slice must succeed on the bytes this produces.
        // Run E rejected every PlayerState because raw entropy bytes
        // weren't valid JSON; this is the regression test that prevents
        // a future refactor from re-breaking the wire/cluster contract.
        for len in [10usize, 50, 100, 500] {
            let mut buf = Vec::new();
            fill_pseudo_user_data(&mut buf, len, 42, 7);
            let parsed: serde_json::Value =
                serde_json::from_slice(&buf).expect("output must parse as JSON");
            assert!(
                parsed.get("d").and_then(|v| v.as_str()).is_some(),
                "envelope must carry the hex payload under key `d`"
            );
        }
    }

    #[test]
    fn fill_pseudo_user_data_clamps_small_len_to_minimum_envelope() {
        // Anything below the 10-byte minimum envelope size produces a
        // 10-byte output. Documented behavior — callers asking for
        // tiny `len` don't get tiny output, they get the smallest valid
        // JSON envelope.
        for len in [1usize, 5, 9] {
            let mut buf = Vec::new();
            fill_pseudo_user_data(&mut buf, len, 1, 1);
            assert_eq!(buf.len(), 10, "len={} must clamp to 10-byte minimum", len);
            let _: serde_json::Value =
                serde_json::from_slice(&buf).expect("clamped output must still be valid JSON");
        }
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
