//! Version-independent update management on the audio UDP socket. Keep this v1 schema stable
//! across audio protocol changes, so incompatible audio nodes can still update each other.
//! Messages carry no executable, path or URL: every node uses its own official release updater.

use hmac::Mac;
use serde::{Deserialize, Serialize};

use crate::protocol::{DecodeError, EncodeError, GroupKey, MAX_DATAGRAM, TAG_LEN};
use crate::{NodeId, Os};

pub const MAGIC: [u8; 4] = *b"SSUP";
pub const VERSION: u8 = 1;
const HEADER_LEN: usize = 21;
pub const MAX_TEXT: usize = 384;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateState {
    Unavailable(String),
    Idle,
    Checking,
    UpToDate,
    Available { build: u32 },
    Downloading { build: u32, received: u64, total: u64 },
    Restarting { build: u32 },
    Failed { error: String, build: Option<u32> },
}

impl UpdateState {
    pub fn installable(&self) -> Option<u32> {
        match self {
            Self::Available { build } | Self::Failed { build: Some(build), .. } => Some(*build),
            _ => None,
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. } | Self::Restarting { .. })
    }

    fn valid(&self) -> bool {
        match self {
            Self::Unavailable(s) | Self::Failed { error: s, .. } => s.len() <= MAX_TEXT,
            Self::Downloading { received, total, .. } => received <= total && *total <= 256 << 20,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub name: String,
    pub os: Os,
    pub version: String,
    pub build: Option<u32>,
    pub protocol: u8,
    pub state: UpdateState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateAction {
    Check,
    /// Install the advertised latest build. A stale request never installs a different build.
    Install { build: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateMessage {
    Status { reply: bool, info: UpdateInfo },
    /// Target session and increasing request number prevent duplicate or stale restarts.
    Request { target: NodeId, session: u64, request: u64, action: UpdateAction },
    Ack { target: NodeId, session: u64, request: u64, error: Option<String> },
    /// Optional v1 extension. Only this request's result can finish its operation; ordinary
    /// status packets may have been sent before the request. Keep the first three layouts intact.
    Result { target: NodeId, session: u64, request: u64, state: UpdateState },
}

impl UpdateMessage {
    fn valid(&self) -> bool {
        match self {
            Self::Status { info, .. } => info.name.len() <= 128 && info.version.len() <= 96 && info.state.valid(),
            Self::Request { request, .. } => *request > 0,
            Self::Ack { request, error, .. } => *request > 0 && error.as_ref().is_none_or(|s| s.len() <= MAX_TEXT),
            Self::Result { request, state, .. } => *request > 0 && state.valid()
                && (!state.busy() || matches!(state, UpdateState::Restarting { .. })),
        }
    }
}

/// Bounds display strings in a datagram without splitting a UTF-8 character.
pub fn bounded(text: &str, max: usize) -> String {
    text[..text.floor_char_boundary(max.min(text.len()))].to_string()
}

pub fn encode(sender: NodeId, session: u64, message: &UpdateMessage, key: &GroupKey, buf: &mut [u8; MAX_DATAGRAM]) -> Result<usize, EncodeError> {
    if !message.valid() {
        return Err(EncodeError::Serialize);
    }
    buf[..4].copy_from_slice(&MAGIC);
    buf[4] = VERSION;
    buf[5..13].copy_from_slice(&sender.to_le_bytes());
    buf[13..HEADER_LEN].copy_from_slice(&session.to_le_bytes());
    let body = postcard::to_slice(message, &mut buf[HEADER_LEN..MAX_DATAGRAM - TAG_LEN]).map_err(|_| EncodeError::TooLarge)?;
    let end = HEADER_LEN + body.len();
    let mut mac = key.mac();
    mac.update(&buf[..end]);
    buf[end..end + TAG_LEN].copy_from_slice(&mac.finalize().into_bytes()[..TAG_LEN]);
    Ok(end + TAG_LEN)
}

pub fn decode(buf: &[u8], key: &GroupKey) -> Result<(NodeId, u64, UpdateMessage), DecodeError> {
    if buf.len() < HEADER_LEN + TAG_LEN {
        return Err(DecodeError::TooShort);
    }
    if buf.len() > MAX_DATAGRAM {
        return Err(DecodeError::Malformed);
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(DecodeError::BadVersion(buf[4]));
    }
    let end = buf.len() - TAG_LEN;
    let mut mac = key.mac();
    mac.update(&buf[..end]);
    mac.verify_truncated_left(&buf[end..]).map_err(|_| DecodeError::BadTag)?;
    let (message, rest): (UpdateMessage, _) = postcard::take_from_bytes(&buf[HEADER_LEN..end]).map_err(|_| DecodeError::Malformed)?;
    if !rest.is_empty() || !message.valid() {
        return Err(DecodeError::Malformed);
    }
    let sender = u64::from_le_bytes(buf[5..13].try_into().expect("8 bytes"));
    let session = u64::from_le_bytes(buf[13..HEADER_LEN].try_into().expect("8 bytes"));
    Ok((sender, session, message))
}
