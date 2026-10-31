//! SoundSync core: wire protocol, clock sync, playout timing, channel layouts and DSP.
//!
//! Everything here is pure and deterministic so it can be unit-tested without audio hardware.

pub mod calib;
pub mod clock;
pub mod config;
pub mod delay;
pub mod jitter;
pub mod layout;
pub mod limiter;
pub mod paths;
pub mod player;
pub mod protocol;
pub mod sync;
pub mod timeline;
pub mod types;

pub use types::{MasterMode, Nanos, NodeId, Os, Role, StreamFormat};
