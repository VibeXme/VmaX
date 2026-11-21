use std::sync::{Arc, Mutex};

use iroh::endpoint::{Connection, ConnectionError};
use tokio::sync::mpsc;

use crate::protocol::InputPacket;

use super::host::MAX_PLAYERS;

const QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Debug, Default)]
pub(super) struct InputInbox {
    slots: Arc<Mutex<[Option<mpsc::Receiver<InputPacket>>; MAX_PLAYERS]>>,
}

impl InputInbox {
    pub(super) fn register(&self, slot: usize) -> mpsc::Sender<InputPacket> {
        let (send, recv) = mpsc::channel(QUEUE_CAPACITY);
        self.slots.lock().unwrap_or_else(|err| err.into_inner())[slot] = Some(recv);
        send
    }

    pub(super) fn clear(&self, slot: usize) {
        self.slots.lock().unwrap_or_else(|err| err.into_inner())[slot] = None;
    }

    pub(super) fn try_recv(&self, slot: usize) -> Option<InputPacket> {
        self.slots
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get_mut(slot)?
            .as_mut()?
            .try_recv()
            .ok()
    }
}

pub(super) async fn receive(
    connection: &Connection,
    inbox: mpsc::Sender<InputPacket>,
) -> Result<(), ConnectionError> {
    let mut latest = None;
    loop {
        let data = connection.read_datagram().await?;
        let Ok(packet) = InputPacket::decode(&data) else {
            continue;
        };
        if latest.is_some_and(|seq| packet.seq <= seq) {
            continue;
        }
        latest = Some(packet.seq);
        let _ = inbox.try_send(packet);
    }
}
