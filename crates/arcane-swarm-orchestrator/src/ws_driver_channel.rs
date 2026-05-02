//! Production `DriverChannel` impl backed by a per-connection mpsc populated
//! by the WebSocket server. The dispatcher's `submit` calls `send(seq, cmd)`;
//! we push `(seq, cmd, ack_tx)` onto the mpsc, the per-connection writer
//! task serializes the command on the wire and parks `ack_tx` in a
//! `pending_acks` map keyed by `seq`. When the driver echoes back a
//! `CommandAck` carrying the same `seq`, the reader resolves the oneshot.

use crate::command_dispatcher::DriverChannel;
use crate::protocol::{CommandAck, OrchestratorCommand};
use tokio::sync::{mpsc, oneshot};

pub type CommandSink = mpsc::Sender<(u64, OrchestratorCommand, oneshot::Sender<CommandAck>)>;

pub struct WsDriverChannel {
    cmd_tx: CommandSink,
}

impl WsDriverChannel {
    pub fn new(cmd_tx: CommandSink) -> Self {
        Self { cmd_tx }
    }
}

impl DriverChannel for WsDriverChannel {
    async fn send(&self, seq: u64, command: OrchestratorCommand) -> Result<CommandAck, String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.cmd_tx
            .send((seq, command, ack_tx))
            .await
            .map_err(|_| "driver write channel closed".to_string())?;
        ack_rx
            .await
            .map_err(|_| "driver ack channel closed".to_string())
    }
}
