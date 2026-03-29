//! Simple deterministic wander model for simulated clients.
//!
//! Shared by both backends so load profile is comparable independent of transport/runtime wiring.

const WORLD_SIZE: f64 = 5000.0;
const WORLD_CENTER: f64 = 2500.0;
const MOVE_SPEED: f64 = 600.0;
const CLUSTER_RADIUS: f64 = 300.0;
const SPREAD_MARGIN: f64 = 200.0;

pub struct Player {
    pub id: uuid::Uuid,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    pub dir_x: f64,
    pub dir_z: f64,
    ticks_until_turn: u32,
}

impl Player {
    /// `entity_id` must match SpacetimeDB action reducers (`all_ids[idx]`) and Arcane wire entity id for the same slot.
    pub fn new(entity_id: uuid::Uuid, idx: u32, total: u32, clustered: bool) -> Self {
        let angle = (idx as f64 / total.max(1) as f64) * std::f64::consts::TAU;
        let radius = if clustered {
            CLUSTER_RADIUS
        } else {
            WORLD_SIZE * 0.35
        };
        Self {
            id: entity_id,
            x: WORLD_CENTER + radius * angle.cos(),
            y: 0.0,
            z: WORLD_CENTER + radius * angle.sin(),
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
            dir_x: angle.cos(),
            dir_z: angle.sin(),
            ticks_until_turn: 60 + (idx % 80),
        }
    }

    pub fn tick(&mut self, tick_dt: f64, clustered: bool) {
        self.ticks_until_turn = self.ticks_until_turn.saturating_sub(1);
        if self.ticks_until_turn == 0 {
            let a =
                (self.id.as_bytes()[0] as f64 * 0.1 + self.x * 0.001).sin() * std::f64::consts::TAU;
            self.dir_x = a.cos();
            self.dir_z = a.sin();
            self.ticks_until_turn = 40 + ((self.id.as_bytes()[1] as u32) % 80);
        }
        let speed = MOVE_SPEED * tick_dt;
        self.vx = self.dir_x * speed;
        self.vz = self.dir_z * speed;
        self.x += self.vx;
        self.z += self.vz;

        let (min, max) = if clustered {
            (WORLD_CENTER - CLUSTER_RADIUS, WORLD_CENTER + CLUSTER_RADIUS)
        } else {
            (SPREAD_MARGIN, WORLD_SIZE - SPREAD_MARGIN)
        };
        if self.x < min {
            self.x = min;
            self.dir_x = self.dir_x.abs();
        }
        if self.x > max {
            self.x = max;
            self.dir_x = -self.dir_x.abs();
        }
        if self.z < min {
            self.z = min;
            self.dir_z = self.dir_z.abs();
        }
        if self.z > max {
            self.z = max;
            self.dir_z = -self.dir_z.abs();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_stays_in_bounds_spread() {
        let mut p = Player::new(uuid::Uuid::nil(), 0, 10, false);
        for _ in 0..200 {
            p.tick(0.05, false);
        }
        assert!(p.x.is_finite() && p.z.is_finite());
        assert!(p.x >= SPREAD_MARGIN && p.x <= WORLD_SIZE - SPREAD_MARGIN);
        assert!(p.z >= SPREAD_MARGIN && p.z <= WORLD_SIZE - SPREAD_MARGIN);
    }
}
