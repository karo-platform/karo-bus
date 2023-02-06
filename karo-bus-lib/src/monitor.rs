use std::sync::Arc;

use async_trait::async_trait;
use bson::Bson;
use log::*;
use serde::Serialize;
use tokio::sync::Mutex;

use karo_common_connection::monitor::{MessageDirection, Monitor as ConnectionMonitor};
use karo_common_rpc::rpc_sender::RpcSender;

/// Monitor message to send. Uses references for cheap construction
#[derive(Serialize, Debug, Clone)]
pub struct MonitorMessage<'a> {
    pub sender: &'a String,
    pub receiver: &'a String,
    /// This should be a serialized [Message]
    pub message: &'a Bson,
    pub direction: MessageDirection,
}

/// Monitor wrapper to pass to connection handles.
/// Uses arc internally, because we need common Option to set incoming
/// monitor connections to all connection handles at once
#[derive(Clone)]
pub(crate) struct Monitor {
    self_name: String,
    peer_name: String,
    sender: Arc<Mutex<Option<RpcSender>>>,
}

impl Monitor {
    pub fn new(self_name: String, peer_name: String) -> Self {
        Self {
            self_name,
            peer_name,
            sender: Arc::new(Mutex::new(None)),
        }
    }

    /// Set monitor handle
    pub async fn set_monitor(&mut self, monitor: RpcSender) {
        *self.sender.lock().await = Some(monitor);
    }
}

#[async_trait]
impl ConnectionMonitor for Monitor {
    async fn message(&mut self, message: &Bson, direction: MessageDirection) {
        let ref mut monitor = *self.sender.lock().await;
        if let Some(monitor) = monitor {
            let (sender, receiver) = match direction {
                MessageDirection::Outgoing => (&self.self_name, &self.peer_name),
                MessageDirection::Incoming => (&self.peer_name, &self.self_name),
            };

            // First we make monitor message, which will be sent as method call parameter...
            let monitor_message = MonitorMessage {
                sender,
                receiver,
                message,
                direction,
            };

            trace!("Sending monitor message: {:?}", message);
            // ..And to call monitor method, we need
            if monitor.call(&message).await.is_err() {
                // Return here if succesfully sent, otherwise reset monitor connection
                return;
            }
        } else {
            return;
        }

        // If reached here, we've failed to send monitor message
        debug!("Monitor disconnected");
        monitor.take();
    }
}
