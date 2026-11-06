//! Platform-independent core of StrangeApeRemoteDesktop (`sard`).
//!
//! Everything in here is plain Rust with no platform APIs beyond `libc`:
//! the wire protocol, the Noise-based handshake, the Reed-Solomon FEC,
//! video frame packetization/reassembly, the reliable control channel and the
//! UDP transport that ties them together. Platform crates (`sard-linux`,
//! `sard-macos`) only deal with capture, encode, decode, presentation and input.

pub mod anywhere;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod fec;
pub mod input;
pub mod log;
pub mod media;
pub mod net;
pub mod png;
pub mod proto;
pub mod rand;
pub mod reliable;
pub mod stats;
pub mod stun;
pub mod turn;
pub mod video;
pub mod web;
pub mod wire;

/// Protocol version spoken on the wire. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default UDP port.
pub const DEFAULT_PORT: u16 = 47800;
