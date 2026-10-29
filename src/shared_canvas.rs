//! The shared canvas: boards that run this program on the same ESP-NOW channel draw on each
//! other's screens.
//!
//! Each board broadcasts its drawing operations with sequence numbers. ESP-NOW broadcasts have no
//! acknowledgements, so every packet repeats the last few operations, and a receiver applies only
//! the ones that it has not applied yet. A board that starts later sees only new drawing.

use embedded_graphics::{
    pixelcolor::{Rgb565, raw::RawU16},
    prelude::*,
};

use crate::display::{HEIGHT, WIDTH};

pub const CHANNEL: u8 = 1;

const MAGIC: [u8; 4] = *b"TPv1";
/// Operations per packet. Each operation goes out in up to this many packets.
const HISTORY: usize = 4;
/// Packets sent after the last operation, for receivers that missed the earlier ones.
const REPEATS: u8 = 3;
/// Magic, session, sequence number of the first operation, operation count.
const HEADER_BYTES: usize = 4 + 4 + 4 + 1;
const OP_BYTES: usize = 12;
pub const PACKET_BYTES: usize = HEADER_BYTES + HISTORY * OP_BYTES;
const MAX_PEERS: usize = 4;
const MAX_RADIUS: i32 = 32;

#[derive(Clone, Copy)]
pub enum Op {
    Line {
        from: (i32, i32),
        to: (i32, i32),
        radius: i32,
        color: Rgb565,
    },
    Clear,
    Restore,
}

impl Op {
    fn encode(&self) -> [u8; OP_BYTES] {
        let mut bytes = [0; OP_BYTES];
        match *self {
            Op::Line {
                from,
                to,
                radius,
                color,
            } => {
                bytes[0] = 0;
                for (i, value) in [from.0, from.1, to.0, to.1].into_iter().enumerate() {
                    bytes[1 + i * 2..3 + i * 2].copy_from_slice(&(value as u16).to_le_bytes());
                }
                bytes[9] = radius.clamp(0, MAX_RADIUS) as u8;
                bytes[10..12].copy_from_slice(&color.into_storage().to_le_bytes());
            }
            Op::Clear => bytes[0] = 1,
            Op::Restore => bytes[0] = 2,
        }
        bytes
    }

    fn decode(bytes: &[u8; OP_BYTES]) -> Option<Self> {
        let coordinate = |i: usize, max: usize| {
            (u16::from_le_bytes([bytes[1 + i * 2], bytes[2 + i * 2]]) as i32).min(max as i32 - 1)
        };
        match bytes[0] {
            0 => Some(Op::Line {
                from: (coordinate(0, WIDTH), coordinate(1, HEIGHT)),
                to: (coordinate(2, WIDTH), coordinate(3, HEIGHT)),
                radius: (bytes[9] as i32).min(MAX_RADIUS),
                color: Rgb565::from(RawU16::new(u16::from_le_bytes([bytes[10], bytes[11]]))),
            }),
            1 => Some(Op::Clear),
            2 => Some(Op::Restore),
            _ => None,
        }
    }
}

/// The local operations that still go out to the other boards.
pub struct Outbox {
    /// Tells the receivers that this board restarted its sequence numbers.
    session: u32,
    /// The sequence number of the next operation.
    next_seq: u32,
    /// The last operations, oldest first.
    recent: [Op; HISTORY],
    len: usize,
    repeats: u8,
}

impl Outbox {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            next_seq: 0,
            recent: [Op::Clear; HISTORY],
            len: 0,
            repeats: 0,
        }
    }

    pub fn push(&mut self, op: Op) {
        if self.len == HISTORY {
            self.recent.rotate_left(1);
            self.len -= 1;
        }
        self.recent[self.len] = op;
        self.len += 1;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.repeats = REPEATS;
    }

    /// The next packet to broadcast, if the last operations need one.
    pub fn next_packet<'a>(&mut self, buffer: &'a mut [u8; PACKET_BYTES]) -> Option<&'a [u8]> {
        if self.repeats == 0 {
            return None;
        }
        self.repeats -= 1;

        let first_seq = self.next_seq.wrapping_sub(self.len as u32);
        buffer[..4].copy_from_slice(&MAGIC);
        buffer[4..8].copy_from_slice(&self.session.to_le_bytes());
        buffer[8..12].copy_from_slice(&first_seq.to_le_bytes());
        buffer[12] = self.len as u8;
        for (op, bytes) in self.recent[..self.len]
            .iter()
            .zip(buffer[HEADER_BYTES..].as_chunks_mut::<OP_BYTES>().0)
        {
            *bytes = op.encode();
        }
        Some(&buffer[..HEADER_BYTES + self.len * OP_BYTES])
    }
}

#[derive(Clone, Copy)]
struct Peer {
    address: [u8; 6],
    session: u32,
    /// The sequence number of the next operation to apply.
    next_seq: u32,
}

/// The other boards that draw on the canvas.
pub struct Peers {
    peers: [Option<Peer>; MAX_PEERS],
    /// The slot that the next new board replaces when all slots are taken.
    next_slot: usize,
}

impl Peers {
    pub fn new() -> Self {
        Self {
            peers: [None; MAX_PEERS],
            next_slot: 0,
        }
    }

    /// Calls `apply` for each operation in `packet` from `address` that was not applied yet.
    pub fn receive(&mut self, address: [u8; 6], packet: &[u8], mut apply: impl FnMut(Op)) {
        if packet.len() < HEADER_BYTES || packet[..4] != MAGIC {
            return;
        }
        let session = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        let first_seq = u32::from_le_bytes(packet[8..12].try_into().unwrap());
        let count = packet[12] as usize;
        if count > HISTORY || packet.len() != HEADER_BYTES + count * OP_BYTES {
            return;
        }

        let peer = self.peer(address, session, first_seq);
        if peer.session != session {
            // The other board restarted.
            peer.session = session;
            peer.next_seq = first_seq;
        }
        for (i, bytes) in packet[HEADER_BYTES..]
            .as_chunks::<OP_BYTES>()
            .0
            .iter()
            .enumerate()
        {
            let seq = first_seq.wrapping_add(i as u32);
            // The packet repeats operations that earlier packets carried.
            if (seq.wrapping_sub(peer.next_seq) as i32) < 0 {
                continue;
            }
            if let Some(op) = Op::decode(bytes) {
                apply(op);
            }
            peer.next_seq = seq.wrapping_add(1);
        }
    }

    /// The peer at `address`. A new peer starts with the given session and operation.
    fn peer(&mut self, address: [u8; 6], session: u32, first_seq: u32) -> &mut Peer {
        let slot = match self
            .peers
            .iter()
            .position(|peer| peer.is_some_and(|peer| peer.address == address))
        {
            Some(slot) => slot,
            None => {
                let slot = self
                    .peers
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or_else(|| {
                        let slot = self.next_slot;
                        self.next_slot = (slot + 1) % MAX_PEERS;
                        slot
                    });
                self.peers[slot] = Some(Peer {
                    address,
                    session,
                    next_seq: first_seq,
                });
                slot
            }
        };
        self.peers[slot].as_mut().unwrap()
    }
}
