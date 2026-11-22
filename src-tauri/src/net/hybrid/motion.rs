use bytes::{Bytes, BytesMut};

use crate::protocol::{InputEvent, InputPacket};

const RELATIVE_SIZE: usize = 26;

#[derive(Default)]
pub(super) struct History {
    previous: [Option<InputPacket>; 2],
}

impl History {
    pub(super) fn encode(&mut self, packet: InputPacket) -> Bytes {
        let current = packet.encode();
        if !relative(&packet) {
            return current;
        }
        // Older peers decode the first packet and ignore trailing bytes.
        let mut bytes = BytesMut::with_capacity(3 * RELATIVE_SIZE);
        bytes.extend_from_slice(&current);
        for previous in self.previous.iter().flatten() {
            bytes.extend_from_slice(&previous.encode());
        }
        self.previous = [self.previous[1], Some(packet)];
        bytes.freeze()
    }
}

pub(super) fn relative(packet: &InputPacket) -> bool {
    matches!(packet.event, InputEvent::MouseMoveRel { .. })
}

pub(super) fn receive(bytes: &[u8], last: &mut Option<u64>, mut emit: impl FnMut(InputPacket)) {
    let Ok(current) = InputPacket::decode(bytes) else {
        return;
    };
    if !relative(&current) {
        emit(current);
        return;
    }
    let mut accept = |packet: InputPacket| {
        if last.is_none_or(|seq| seq < packet.seq) {
            *last = Some(packet.seq);
            emit(packet);
        }
    };
    if let Some(history) = bytes.get(RELATIVE_SIZE..)
        && history.len() <= 2 * RELATIVE_SIZE
    {
        for bytes in history.chunks_exact(RELATIVE_SIZE) {
            if let Ok(previous) = InputPacket::decode(bytes)
                && relative(&previous)
                && previous.seq < current.seq
            {
                accept(previous);
            }
        }
    }
    accept(current);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_dropped_relative_motion_once() {
        let mut history = History::default();
        let mut encode = |seq| {
            history.encode(InputPacket {
                seq,
                client_ts_us: 0,
                event: InputEvent::MouseMoveRel { dx: 1, dy: -1 },
            })
        };
        let first = encode(1);
        let lost = encode(2);
        let third = encode(3);
        assert_eq!(InputPacket::decode(&third).unwrap().seq, 3);
        let mut last = None;
        let mut applied = Vec::new();
        for bytes in [first, third.clone(), lost, third] {
            receive(&bytes, &mut last, |packet| applied.push(packet.seq));
        }
        assert_eq!(applied, [1, 2, 3]);
    }
}
