use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type DriverId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DriverMessage {
    #[serde(rename = "register")]
    Register(RegisterRequest),
    #[serde(rename = "heartbeat")]
    Heartbeat(HeartbeatRequest),
    #[serde(rename = "deregister")]
    Deregister(DeregisterRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub capabilities: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub driver_id: DriverId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeregisterRequest {
    pub driver_id: DriverId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OrchestratorResponse {
    #[serde(rename = "ack")]
    Ack(AckResponse),
    #[serde(rename = "register_rejected")]
    RegisterRejected(RegisterRejectedResponse),
    #[serde(rename = "error")]
    Error(ErrorResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckResponse {
    pub driver_id: Option<DriverId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRejectedResponse {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub message: String,
}
