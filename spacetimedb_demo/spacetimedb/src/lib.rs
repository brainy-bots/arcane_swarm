//! SpacetimeDB module for Arcane demo.
//!
//! Tables:
//!   - Entity: position only (subscribed by clients)
//!   - PlayerInput: direction per entity (private — no subscription fanout)
//!   - Inventory, GameEvent: as before
//!
//! Intended pattern (per SpacetimeDB docs): clients send input via update_player_input
//! (writes to PlayerInput); scheduled physics_tick reads PlayerInput, updates Entity.
//! Only physics writes to Entity, so one wave of subscription fanout per tick.

use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table};
use std::time::Duration;

const WORLD_SIZE: f64 = 5000.0;
const PHYSICS_SPEED: f64 = 600.0;
const PHYSICS_DT: f64 = 0.05; // 20 Hz

// ── Entity table (public: clients subscribe) ───────────────────────────────
// Position only. Only the scheduled physics_tick writes here in server-physics mode.

#[table(accessor = entity, public)]
pub struct Entity {
    #[primary_key]
    pub entity_id: spacetimedb::Uuid,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

// ── PlayerInput table (private: no client subscription, no fanout) ──────────
// Input writes go here; physics_tick reads and applies. Clients never subscribe.

#[table(accessor = player_input)]
pub struct PlayerInput {
    #[primary_key]
    pub entity_id: spacetimedb::Uuid,
    pub dir_x: f64,
    pub dir_z: f64,
}

// ── Inventory table ──────────────────────────────────────────────────────

#[table(accessor = inventory, public,
    index(accessor = owner_item, btree(columns = [owner_id, item_type])),
)]
pub struct Inventory {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    pub owner_id: spacetimedb::Uuid,
    pub item_type: u32,
    pub quantity: u32,
}

// ── GameEvent table ──────────────────────────────────────────────────────

#[table(accessor = game_event, public)]
pub struct GameEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    pub actor_id: spacetimedb::Uuid,
    pub target_id: spacetimedb::Uuid,
    pub event_type: u32,
    pub timestamp_us: i64,
}

// ── Init ─────────────────────────────────────────────────────────────────

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    log::info!("arcane-demo SpacetimeDB module initialized");
    #[cfg(feature = "server_physics")]
    {
        ctx.db
            .physics_timer()
            .insert(PhysicsTimer {
                scheduled_id: 0,
                scheduled_at: ScheduleAt::from(Duration::from_millis(50)),
            });
    }
}

// ── Scheduled physics (optional, feature-gated) ─────────────────────────

#[cfg(feature = "server_physics")]
#[table(accessor = physics_timer, scheduled(physics_tick))]
pub struct PhysicsTimer {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: spacetimedb::ScheduleAt,
}

#[cfg(feature = "server_physics")]
#[reducer]
pub fn physics_tick(ctx: &ReducerContext, _timer: PhysicsTimer) -> Result<(), String> {
    let min = 200.0;
    let max = WORLD_SIZE - 200.0;
    let step = PHYSICS_SPEED * PHYSICS_DT;
    for mut entity in ctx.db.entity().iter() {
        let (dx, dz) = ctx
            .db
            .player_input()
            .entity_id()
            .find(&entity.entity_id)
            .map(|inp| (inp.dir_x * step, inp.dir_z * step))
            .unwrap_or((0.0, 0.0));
        entity.x = (entity.x + dx).clamp(min, max);
        entity.z = (entity.z + dz).clamp(min, max);
        ctx.db.entity().entity_id().update(entity);
    }
    Ok(())
}

// ── Entity reducers ──────────────────────────────────────────────────────

/// Insert or update a player's position. Used for initial spawn and for
/// non–server-physics mode (client sends position every tick).
#[reducer]
pub fn update_player(ctx: &ReducerContext, entity: Entity) -> Result<(), String> {
    if ctx.db.entity().entity_id().find(&entity.entity_id).is_some() {
        ctx.db.entity().entity_id().update(entity);
    } else {
        ctx.db.entity().insert(entity);
    }
    Ok(())
}

/// Store movement input (private PlayerInput table). No subscription fanout.
/// physics_tick reads this and writes only to Entity — one wave of fanout per tick.
#[reducer]
pub fn update_player_input(
    ctx: &ReducerContext,
    entity_id: spacetimedb::Uuid,
    dir_x: f64,
    dir_z: f64,
) -> Result<(), String> {
    if let Some(mut row) = ctx.db.player_input().entity_id().find(&entity_id) {
        row.dir_x = dir_x;
        row.dir_z = dir_z;
        ctx.db.player_input().entity_id().update(row);
    } else {
        ctx.db.player_input().insert(PlayerInput {
            entity_id,
            dir_x,
            dir_z,
        });
    }
    Ok(())
}

/// Remove a player entity by ID. Also clears PlayerInput so we don't leak rows.
#[reducer]
pub fn remove_player_by_id(ctx: &ReducerContext, entity_id: spacetimedb::Uuid) -> Result<(), String> {
    ctx.db.entity().entity_id().delete(&entity_id);
    ctx.db.player_input().entity_id().delete(&entity_id);
    Ok(())
}

/// Bulk-replace all entities (used for initial world setup).
#[reducer]
pub fn set_entities(ctx: &ReducerContext, entities: Vec<Entity>) -> Result<(), String> {
    let ids: Vec<_> = ctx.db.entity().iter().map(|r| r.entity_id).collect();
    for id in ids {
        ctx.db.entity().entity_id().delete(&id);
    }
    for e in entities {
        ctx.db.entity().insert(e);
    }
    Ok(())
}

// ── Inventory reducers ───────────────────────────────────────────────────

/// Add items to a player's inventory. Uses the composite index for lookup
/// and update-in-place to minimize subscription events.
#[reducer]
pub fn pickup_item(ctx: &ReducerContext, owner_id: spacetimedb::Uuid, item_type: u32, quantity: u32) -> Result<(), String> {
    let mut found = false;
    for mut row in ctx.db.inventory().owner_item().filter((owner_id, item_type)) {
        row.quantity += quantity;
        ctx.db.inventory().row_id().update(row);
        found = true;
        break;
    }
    if !found {
        ctx.db.inventory().insert(Inventory {
            row_id: 0,
            owner_id,
            item_type,
            quantity,
        });
    }
    Ok(())
}

/// Consume one unit of an item. Removes the row if quantity reaches 0.
#[reducer]
pub fn use_item(ctx: &ReducerContext, owner_id: spacetimedb::Uuid, item_type: u32) -> Result<(), String> {
    for mut row in ctx.db.inventory().owner_item().filter((owner_id, item_type)) {
        if row.quantity > 1 {
            row.quantity -= 1;
            ctx.db.inventory().row_id().update(row);
        } else {
            ctx.db.inventory().row_id().delete(&row.row_id);
        }
        break;
    }
    Ok(())
}

// ── Interaction / event reducers ─────────────────────────────────────────

/// Log a player interaction with a real server-side timestamp.
#[reducer]
pub fn player_interact(ctx: &ReducerContext, actor_id: spacetimedb::Uuid, target_id: spacetimedb::Uuid, event_type: u32) -> Result<(), String> {
    ctx.db.game_event().insert(GameEvent {
        event_id: 0,
        actor_id,
        target_id,
        event_type,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });
    Ok(())
}

/// Remove events older than `max_age_secs`. Call periodically to bound table growth.
#[reducer]
pub fn cleanup_old_events(ctx: &ReducerContext, max_age_secs: u64) -> Result<(), String> {
    let cutoff = ctx.timestamp.to_micros_since_unix_epoch() - (max_age_secs as i64 * 1_000_000);
    let stale: Vec<u64> = ctx.db.game_event().iter()
        .filter(|e| e.timestamp_us < cutoff)
        .map(|e| e.event_id)
        .collect();
    for id in stale {
        ctx.db.game_event().event_id().delete(&id);
    }
    Ok(())
}
