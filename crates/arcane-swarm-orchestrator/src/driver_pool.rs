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

    pub async fn register(
        &self,
        capabilities: Value,
    ) -> Result<DriverId, String> {
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
}
