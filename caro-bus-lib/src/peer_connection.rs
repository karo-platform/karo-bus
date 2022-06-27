use std::{error::Error, fmt::Debug, sync::Arc};

use log::*;
use parking_lot::RwLock;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    net::UnixStream,
    sync::{
        broadcast::Receiver as BroadcastReceiver,
        mpsc::{self, Receiver, Sender},
    },
};

use crate::{
    peer_handle::Peer,
    utils::{self, TaskChannel},
};
use caro_bus_common::{
    errors::Error as BusError,
    messages::{self, IntoMessage, Message, MessageBody, Response},
};

type Shared<T> = Arc<RwLock<T>>;

/// P2p service connection handle
#[derive(Clone)]
pub struct PeerConnection {
    /// Own service name
    service_name: Shared<String>,
    /// Peer service name
    peer_service_name: Shared<String>,
    /// Sender to forward calls to the service
    service_tx: TaskChannel,
    /// Sender to make calls into the task
    task_tx: TaskChannel,
    /// Sender to shutdown peer connection
    shutdown_tx: Sender<()>,
}

impl PeerConnection {
    /// Create new service handle and start tokio task to handle incoming messages from the peer
    pub fn new(
        service_name: String,
        peer_service_name: String,
        socket: UnixStream,
        service_tx: TaskChannel,
    ) -> Self {
        let (task_tx, mut task_rx) = mpsc::channel(32);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);

        let mut this = Self {
            service_name: Arc::new(RwLock::new(service_name)),
            peer_service_name: Arc::new(RwLock::new(peer_service_name.clone())),
            service_tx: service_tx.clone(),
            task_tx,
            shutdown_tx,
        };
        let result = this.clone();

        tokio::spawn(async move {
            let mut peer_handle = Peer::new(peer_service_name, socket, service_tx);

            loop {
                tokio::select! {
                    // Read incoming message from the peer
                    message = peer_handle.read_message() => {
                        // Peer handle resolves call itself. If message returned, redirect to
                        // the service connection
                        let response = this.handle_peer_message(message).await;

                        let (callback_tx, mut callback_rx) = mpsc::channel(1);
                        peer_handle.write_message(response, callback_tx).await;
                        let _ = callback_rx.recv();
                    },
                    // Handle method calls
                    Some((request, callback_tx)) = task_rx.recv() => {
                        trace!("Peer task message: {:?}", request);

                        peer_handle.write_message(request, callback_tx).await;
                    },
                    Some(_) = shutdown_rx.recv() => {
                        drop(peer_handle);
                        return
                    }
                };
            }
        });

        result
    }

    /// Remote method call
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method_name: &String,
        params: &P,
    ) -> Result<R, Box<dyn Error + Sync + Send>> {
        let message = Message::new_call(
            self.peer_service_name.read().clone(),
            method_name.clone(),
            params,
        );

        // Send method call request
        let response = utils::call_task(&self.task_tx, message).await;

        match response {
            Ok(message) => match message.body() {
                // Succesfully performed remote method call
                MessageBody::Response(Response::Return(data)) => {
                    match bson::from_bson::<R>(data.clone()) {
                        Ok(data) => Ok(data),
                        Err(err) => {
                            error!("Can't deserialize method response: {}", err.to_string());
                            Err(Box::new(BusError::InvalidResponse))
                        }
                    }
                }
                // Got an error from the peer
                MessageBody::Response(Response::Error(err)) => {
                    warn!(
                        "Failed to perform a call to `{}::{}`: {}",
                        self.peer_service_name.read(),
                        method_name,
                        err.to_string()
                    );
                    Err(Box::new(err.clone()))
                }
                // Invalid protocol
                r => {
                    error!("Invalid Ok response for a method call: {:?}", r);
                    Err(Box::new(BusError::InvalidMessage))
                }
            },
            // Network error
            Err(e) => {
                error!("Ivalid error response from a method call: {:?}", e);
                Err(e)
            }
        }
    }

    /// Remote signal subscription
    pub async fn subscribe<T: DeserializeOwned>(
        &mut self,
        signal_name: &String,
        callback: impl Fn(&T) + Send + 'static,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let message =
            Message::new_subscription(self.service_name.read().clone(), signal_name.clone());

        let (response, rx) = self.make_subscription_call(message, signal_name).await?;

        match response.body() {
            // Succesfully performed remote method call
            MessageBody::Response(Response::Ok) => {
                debug!("Succesfully subscribed to the signal `{}`", signal_name);

                PeerConnection::start_subscription_receiving_task(signal_name, rx, callback);

                Ok(())
            }
            // Invalid protocol
            r => {
                error!("Invalid Ok response for a signal subscription: {:?}", r);
                Err(Box::new(BusError::InvalidMessage))
            }
        }
    }

    /// Start watching remote state changes
    /// "Returns" current state value
    pub async fn watch<T: DeserializeOwned>(
        &mut self,
        state_name: &String,
        callback: impl Fn(&T) + Send + 'static,
    ) -> Result<T, Box<dyn Error + Sync + Send>> {
        let message = Message::new_watch(self.service_name.read().clone(), state_name.clone());

        let (response, rx) = self.make_subscription_call(message, state_name).await?;

        match response.body() {
            // Succesfully performed remote method call
            MessageBody::Response(Response::StateChanged(data)) => {
                let state = match bson::from_bson::<T>(data.clone()) {
                    Ok(data) => Ok(data),
                    Err(err) => {
                        error!("Can't deserialize state response: {}", err.to_string());
                        Err(Box::new(BusError::InvalidResponse))
                    }
                }?;

                debug!("Succesfully started watching state `{}`", state_name);

                PeerConnection::start_subscription_receiving_task(state_name, rx, callback);

                Ok(state)
            }
            // Invalid protocol
            r => {
                error!("Invalid Ok response for a signal subscription: {:?}", r);
                Err(Box::new(BusError::InvalidMessage))
            }
        }
    }

    /// Make subscription call and get result
    /// Used for both: signals and states
    pub async fn make_subscription_call(
        &mut self,
        message: Message,
        signal_name: &String,
    ) -> Result<(Message, Receiver<Message>), Box<dyn Error + Sync + Send>> {
        // This is a tricky one. First we use channel to read subscription status, and after that
        // for incomin signal emission
        let (tx, mut rx) = mpsc::channel(10);

        // Send subscription request
        self.task_tx.send((message, tx)).await?;

        let response = rx.recv().await.unwrap();

        match response.body() {
            // Got an error from the peer
            MessageBody::Response(Response::Error(err)) => {
                warn!(
                    "Failed to subscribe to `{}::{}`: {}",
                    self.peer_service_name.read(),
                    signal_name,
                    err.to_string()
                );
                Err(Box::new(err.clone()))
            }
            // Invalid protocol
            _ => Ok((response, rx)),
        }
    }

    /// Start task to receive signals emission, state changes and calling user callback
    fn start_subscription_receiving_task<T: DeserializeOwned>(
        signal_name: &String,
        mut receiver: Receiver<Message>,
        callback: impl Fn(&T) + Send + 'static,
    ) {
        let signal_name = signal_name.clone();

        // Start listening to signal emissions
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Some(message) => {
                        match message.body() {
                            // Signal
                            MessageBody::Response(Response::Signal(value)) => {
                                match bson::from_bson::<T>(value.clone()) {
                                    Ok(value) => {
                                        // Call back
                                        callback(&value);
                                    }
                                    Err(err) => {
                                        error!(
                                            "Failed to deserialize signal value: {}",
                                            err.to_string()
                                        );
                                    }
                                }
                            }
                            MessageBody::Response(Response::StateChanged(value)) => {
                                match bson::from_bson::<T>(value.clone()) {
                                    Ok(value) => {
                                        // Call back
                                        callback(&value);
                                    }
                                    Err(err) => {
                                        error!(
                                            "Failed to deserialize state value: {}",
                                            err.to_string()
                                        );
                                    }
                                }
                            }
                            m => {
                                error!("Invalid message inside signal handling code: {:?}", m);
                            }
                        }
                    }
                    None => {
                        error!(
                            "Failed to listen to signal `{}` subscription. Cancelling",
                            signal_name
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Start subscription task, which polls signal Receiver and sends peer message
    /// if emited
    pub(crate) fn start_signal_sending_task(
        &self,
        mut signal_receiver: BroadcastReceiver<Message>,
        seq: u64,
    ) {
        let self_tx = self.task_tx.clone();

        tokio::spawn(async move {
            loop {
                // Wait for signal emission
                match signal_receiver.recv().await {
                    Ok(mut message) => {
                        // Replace seq with subscription seq
                        message.update_seq(seq);

                        // Call self task to send signal message
                        if let Err(_) = utils::call_task(&self_tx, message).await {
                            error!("Failed to send signal to a subscriber. Removing subscriber");
                            return;
                        }
                    }
                    Err(err) => {
                        error!("Signal receiver error: {:?}", err);
                        return;
                    }
                }
            }
        });
    }

    /// Handle messages from the peer
    async fn handle_peer_message(&mut self, message: messages::Message) -> Message {
        trace!("Incoming client message: {:?}", message);

        let response_seq = message.seq();

        match utils::call_task(&self.service_tx, message).await {
            Ok(response) => response,
            Err(err) => {
                warn!(
                    "Error return as a result of peer message handling: {}",
                    err.to_string()
                );

                BusError::Internal.into_message(response_seq)
            }
        }
    }

    pub async fn close(&mut self) {
        let self_name = self.peer_service_name.read().clone();
        debug!(
            "Shutting down peer connection to `{:?}`",
            self.peer_service_name.read()
        );

        let _ = utils::call_task(
            &self.service_tx,
            Response::Shutdown(self_name.clone()).into_message(0),
        );
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        let shutdown_tx = self.shutdown_tx.clone();

        tokio::spawn(async move {
            let _ = shutdown_tx.send(()).await;
        });
    }
}

impl Debug for PeerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Peer connection to {}", self.peer_service_name.read())
    }
}
