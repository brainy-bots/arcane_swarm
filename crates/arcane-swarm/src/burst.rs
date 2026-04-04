//! Deterministic burst-pattern helpers shared across backends.

#[derive(Clone, Copy, Debug)]
pub struct BurstConfig {
    pub enabled: bool,
    pub burst_period_secs: u64,
    pub burst_cohort_percent: u32,
    pub burst_actions_per_player: u32,
    pub burst_window_ms: u64,
    pub zone_event_period_secs: u64,
    pub zone_event_window_ms: u64,
}

impl Default for BurstConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            burst_period_secs: 30,
            burst_cohort_percent: 20,
            burst_actions_per_player: 10,
            burst_window_ms: 500,
            zone_event_period_secs: 30,
            zone_event_window_ms: 500,
        }
    }
}

pub fn is_zone_event_active(now_ms: u64, cfg: BurstConfig) -> bool {
    if !cfg.enabled || cfg.zone_event_period_secs == 0 || cfg.zone_event_window_ms == 0 {
        return false;
    }
    let period_ms = cfg.zone_event_period_secs.saturating_mul(1000);
    if period_ms == 0 {
        return false;
    }
    now_ms % period_ms < cfg.zone_event_window_ms
}

pub fn burst_actions_to_emit(player_idx: u32, now_ms: u64, cfg: BurstConfig) -> u32 {
    if !cfg.enabled
        || cfg.burst_period_secs == 0
        || cfg.burst_window_ms == 0
        || cfg.burst_actions_per_player == 0
    {
        return 0;
    }
    let period_ms = cfg.burst_period_secs.saturating_mul(1000);
    if period_ms == 0 {
        return 0;
    }
    if now_ms % period_ms >= cfg.burst_window_ms {
        return 0;
    }
    let cohort = cfg.burst_cohort_percent.min(100);
    if cohort == 0 {
        return 0;
    }
    // Deterministic cohort membership rotates every burst period.
    let burst_index = now_ms / period_ms;
    let selector = (player_idx as u64 + burst_index) % 100;
    if selector < cohort as u64 {
        cfg.burst_actions_per_player
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_deterministic_for_same_time() {
        let cfg = BurstConfig {
            enabled: true,
            ..BurstConfig::default()
        };
        let a = burst_actions_to_emit(7, 30_100, cfg);
        let b = burst_actions_to_emit(7, 30_100, cfg);
        assert_eq!(a, b);
    }

    #[test]
    fn zone_event_window_matches_period() {
        let cfg = BurstConfig {
            enabled: true,
            ..BurstConfig::default()
        };
        assert!(is_zone_event_active(30_100, cfg));
        assert!(!is_zone_event_active(31_000, cfg));
    }
}
