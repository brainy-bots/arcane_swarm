//! Backend-agnostic orchestration helpers for control-mode player scaling.
//!
//! These helpers are intentionally pure and testable with fake backends so
//! control-plane behavior can be validated without live Arcane/SpacetimeDB services.

/// Minimal backend surface needed by scaling orchestration.
pub trait OrchestrationBackend {
    fn spawn_player(&mut self, idx: usize, desired_total: u32);
    fn stop_player(&mut self, idx: usize);
}

/// Reconciles desired player target against currently spawned count.
///
/// Returns the new `current_spawned` value.
pub fn reconcile_target_players<B: OrchestrationBackend>(
    backend: &mut B,
    current_spawned: usize,
    desired_players: u32,
    max_players: u32,
) -> usize {
    let target = desired_players.min(max_players) as usize;
    if target > current_spawned {
        for idx in current_spawned..target {
            backend.spawn_player(idx, desired_players);
        }
        target
    } else if target < current_spawned {
        for idx in target..current_spawned {
            backend.stop_player(idx);
        }
        target
    } else {
        current_spawned
    }
}

#[cfg(test)]
mod tests {
    use super::{reconcile_target_players, OrchestrationBackend};

    #[derive(Default)]
    struct FakeBackend {
        spawned: Vec<(usize, u32)>,
        stopped: Vec<usize>,
    }

    impl OrchestrationBackend for FakeBackend {
        fn spawn_player(&mut self, idx: usize, desired_total: u32) {
            self.spawned.push((idx, desired_total));
        }

        fn stop_player(&mut self, idx: usize) {
            self.stopped.push(idx);
        }
    }

    #[test]
    fn scales_up_spawning_new_indices_only() {
        let mut backend = FakeBackend::default();
        let new_count = reconcile_target_players(&mut backend, 2, 5, 10);

        assert_eq!(new_count, 5);
        assert_eq!(backend.spawned, vec![(2, 5), (3, 5), (4, 5)]);
        assert!(backend.stopped.is_empty());
    }

    #[test]
    fn scales_down_stopping_excess_indices_only() {
        let mut backend = FakeBackend::default();
        let new_count = reconcile_target_players(&mut backend, 5, 2, 10);

        assert_eq!(new_count, 2);
        assert_eq!(backend.stopped, vec![2, 3, 4]);
        assert!(backend.spawned.is_empty());
    }

    #[test]
    fn clamps_to_max_players_before_scaling() {
        let mut backend = FakeBackend::default();
        let new_count = reconcile_target_players(&mut backend, 1, 50, 3);

        assert_eq!(new_count, 3);
        assert_eq!(backend.spawned, vec![(1, 50), (2, 50)]);
    }

    #[test]
    fn no_ops_when_already_at_target() {
        let mut backend = FakeBackend::default();
        let new_count = reconcile_target_players(&mut backend, 4, 4, 10);

        assert_eq!(new_count, 4);
        assert!(backend.spawned.is_empty());
        assert!(backend.stopped.is_empty());
    }
}
