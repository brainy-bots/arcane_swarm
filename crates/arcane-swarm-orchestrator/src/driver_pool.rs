use crate::protocol::DriverId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverState {
    Active,
    Stale,
    Failed,
}

#[derive(Debug, Clone)]
pub struct DriverEntry {
    pub id: DriverId,
    pub state: DriverState,
    pub last_heartbeat: Instant,
    pub capabilities: Value,
}

pub struct DriverPool {
    drivers: Arc<RwLock<HashMap<DriverId, DriverEntry>>>,
    #[allow(dead_code)]
    heartbeat_interval: Duration,
    stale_threshold: Duration,
    max_drivers: usize,
}

impl DriverPool {
    pub fn new(
        heartbeat_interval: Duration,
        stale_threshold: Duration,
        max_drivers: usize,
    ) -> Self {
        Self {
            drivers: Arc::new(RwLock::new(HashMap::new())),
            heartbeat_interval,
            stale_threshold,
            max_drivers,
        }
    }

    pub async fn register(&self, capabilities: Value) -> Result<DriverId, String> {
        let mut drivers = self.drivers.write().await;

        if drivers.len() >= self.max_drivers {
            return Err(format!(
                "Pool at capacity: {} drivers (max: {})",
                drivers.len(),
                self.max_drivers
            ));
        }

        let driver_id = DriverId::new_v4();
        let entry = DriverEntry {
            id: driver_id,
            state: DriverState::Active,
            last_heartbeat: Instant::now(),
            capabilities,
        };

        drivers.insert(driver_id, entry);
        Ok(driver_id)
    }

    pub async fn heartbeat(&self, driver_id: DriverId) -> Result<(), String> {
        let mut drivers = self.drivers.write().await;

        let entry = drivers
            .get_mut(&driver_id)
            .ok_or_else(|| format!("Driver {} not found", driver_id))?;

        entry.last_heartbeat = Instant::now();
        entry.state = DriverState::Active;
        Ok(())
    }

    pub async fn deregister(&self, driver_id: DriverId) -> Result<(), String> {
        let mut drivers = self.drivers.write().await;
        drivers
            .remove(&driver_id)
            .ok_or_else(|| format!("Driver {} not found", driver_id))?;
        Ok(())
    }

    pub async fn mark_stale_drivers(&self) {
        let mut drivers = self.drivers.write().await;
        let now = Instant::now();

        for entry in drivers.values_mut() {
            if entry.state == DriverState::Active {
                let elapsed = now - entry.last_heartbeat;
                if elapsed > self.stale_threshold {
                    entry.state = DriverState::Stale;
                }
            }
        }
    }

    pub async fn len(&self) -> usize {
        self.drivers.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.drivers.read().await.is_empty()
    }

    pub async fn contains(&self, driver_id: DriverId) -> bool {
        self.drivers.read().await.contains_key(&driver_id)
    }

    pub async fn get_state(&self, driver_id: DriverId) -> Option<DriverState> {
        self.drivers.read().await.get(&driver_id).map(|e| e.state)
    }

    /// Snapshot every driver's current entry. Cheap clone — used by the
    /// telemetry source to embed fleet state in each broadcast snapshot.
    pub async fn snapshot(&self) -> Vec<DriverEntry> {
        self.drivers.read().await.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn register_succeeds_when_pool_has_capacity() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        let capabilities = json!({"platform": "linux"});
        let result = pool.register(capabilities).await;

        assert!(result.is_ok());
        assert_eq!(pool.len().await, 1);
    }

    #[tokio::test]
    async fn register_rejects_when_pool_at_capacity() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 2);

        let _cap1 = pool.register(json!({"platform": "linux"})).await.unwrap();
        let _cap2 = pool.register(json!({"platform": "windows"})).await.unwrap();

        assert_eq!(pool.len().await, 2);

        let result = pool.register(json!({"platform": "darwin"})).await;

        assert!(result.is_err());
        assert_eq!(pool.len().await, 2);
    }

    #[tokio::test]
    async fn register_with_same_driver_id_replaces_entry() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        let cap1 = pool
            .register(json!({"platform": "linux", "v": 1}))
            .await
            .unwrap();
        let initial_len = pool.len().await;

        let cap2 = pool
            .register(json!({"platform": "linux", "v": 2}))
            .await
            .unwrap();
        let final_len = pool.len().await;

        assert!(cap1 != cap2);
        assert_eq!(initial_len, 1);
        assert_eq!(final_len, 2);
    }

    #[tokio::test]
    async fn heartbeat_updates_timestamp_and_state() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        let result = pool.heartbeat(driver_id).await;

        assert!(result.is_ok());
        assert_eq!(pool.get_state(driver_id).await, Some(DriverState::Active));
    }

    #[tokio::test]
    async fn heartbeat_for_unknown_driver_returns_error() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);
        let unknown_id = DriverId::new_v4();

        let result = pool.heartbeat(unknown_id).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn deregister_removes_entry() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        assert_eq!(pool.len().await, 1);
        assert!(pool.contains(driver_id).await);

        let result = pool.deregister(driver_id).await;

        assert!(result.is_ok());
        assert_eq!(pool.len().await, 0);
        assert!(!pool.contains(driver_id).await);
    }

    #[tokio::test]
    async fn deregister_for_unknown_driver_returns_error() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);
        let unknown_id = DriverId::new_v4();

        let result = pool.deregister(unknown_id).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn mark_stale_drivers_transitions_expired_to_stale() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(100), 10);

        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        assert_eq!(pool.get_state(driver_id).await, Some(DriverState::Active));

        tokio::time::sleep(Duration::from_millis(150)).await;

        pool.mark_stale_drivers().await;

        assert_eq!(pool.get_state(driver_id).await, Some(DriverState::Stale));
    }

    #[tokio::test]
    async fn mark_stale_drivers_leaves_recent_active() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(200), 10);

        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        assert_eq!(pool.get_state(driver_id).await, Some(DriverState::Active));

        tokio::time::sleep(Duration::from_millis(50)).await;

        pool.mark_stale_drivers().await;

        assert_eq!(pool.get_state(driver_id).await, Some(DriverState::Active));
    }

    #[tokio::test]
    async fn pool_is_empty_initially() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        assert!(pool.is_empty().await);
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn contains_returns_true_for_registered_driver() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);

        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();

        assert!(pool.contains(driver_id).await);
    }

    #[tokio::test]
    async fn contains_returns_false_for_unknown_driver() {
        let pool = DriverPool::new(Duration::from_millis(50), Duration::from_millis(150), 10);
        let unknown_id = DriverId::new_v4();

        assert!(!pool.contains(unknown_id).await);
    }
}
