use std::sync::{Arc, Mutex};

use bytes::Bytes;
use iroh::endpoint::Connection;

use super::host::MAX_PLAYERS;

#[derive(Clone, Debug, Default)]
pub(super) struct MediaFanout {
    connections: Arc<Mutex<[Option<Connection>; MAX_PLAYERS]>>,
}

impl MediaFanout {
    pub(super) fn set(&self, slot: usize, connection: Option<Connection>) {
        self.connections
            .lock()
            .unwrap_or_else(|err| err.into_inner())[slot] = connection;
    }

    pub(super) fn active_players(&self) -> usize {
        self.connections
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .flatten()
            .count()
    }

    pub(super) fn max_datagram_size(&self) -> Option<usize> {
        self.connections
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .flatten()
            .filter_map(Connection::max_datagram_size)
            .min()
    }

    /// Queues the same packet batch on each connection without waiting for space.
    /// Returns the number of players for whom the entire batch was queued.
    pub(super) fn broadcast(&self, packets: &[Bytes]) -> usize {
        let Some(max_packet_len) = packets.iter().map(Bytes::len).max() else {
            return 0;
        };
        let connections = self
            .connections
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        connections
            .iter()
            .flatten()
            .filter(|connection| {
                connection.max_datagram_size().is_some_and(|size| size >= max_packet_len)
                    && matches!(connection.send_many_datagrams(packets), Ok(queued) if queued == packets.len())
            })
            .count()
    }
}
