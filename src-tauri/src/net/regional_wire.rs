//! Wire messages shared by the app and the standalone regional server.
use serde::{Deserialize, Serialize};

pub const MAX_SIGNAL_FRAME: usize = 33 * 1024;
pub const ROOM_CODE_LENGTH: usize = 16;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Probe,
    Register {
        host_token: String,
        invite: String,
    },
    Lookup {
        room: String,
    },
    PairHost {
        room: String,
        lease: String,
        pair: String,
    },
    PairViewer {
        room: String,
        pair: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ready,
    Registered { room: String, lease: String },
    Invite { invite: String },
    Paired { turn: TurnGrant },
    Error { message: String },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnGrant {
    pub domain: String,
    pub udp_port: u16,
    pub tls_port: u16,
    pub username: String,
    pub password: String,
    pub expires_at: u64,
}
