//! Wire protocol definitions.
//!
//! ```text
//! UDP datagram
//! ├─ 0x01 HANDSHAKE_INIT  [type][sender_idx u32][noise IK msg1]
//! ├─ 0x02 HANDSHAKE_RESP  [type][sender_idx u32][receiver_idx u32][noise IK msg2]
//! ├─ 0x03 DATA            [type][receiver_idx u32][counter u64][AEAD(payload)][tag 16]
//! ├─ 0x04 PAIR_INIT       [type][sender_idx u32][noise NNpsk0 msg1]
//! └─ 0x05 PAIR_RESP       [type][sender_idx u32][receiver_idx u32][noise NNpsk0 msg2]
//! ```
//!
//! Every DATA packet is addressed by the *receiver's* session index (chosen by
//! the receiver during the handshake), so a future relay/router can forward
//! packets by index without decrypting anything, and peers can roam between
//! addresses (the latest authenticated source address wins).
//!
//! The decrypted payload starts with a message type byte, see [`msg`].

use crate::wire::{Put, Reader, Result, WireError};

pub mod packet {
    pub const HANDSHAKE_INIT: u8 = 0x01;
    pub const HANDSHAKE_RESP: u8 = 0x02;
    pub const DATA: u8 = 0x03;
    pub const PAIR_INIT: u8 = 0x04;
    pub const PAIR_RESP: u8 = 0x05;
    /// type + receiver index + counter
    pub const DATA_HEADER_LEN: usize = 1 + 4 + 8;
    /// Bytes added to every payload by the DATA framing.
    pub const DATA_OVERHEAD: usize = DATA_HEADER_LEN + crate::crypto::TAG_LEN;
}

/// Inner message types.
pub mod msg {
    /// Video FEC shard (see `video`).
    pub const VIDEO: u8 = 0x01;
    /// Audio packet.
    pub const AUDIO: u8 = 0x02;
    /// Reliable ordered channel segment (see `reliable`).
    pub const RELIABLE: u8 = 0x03;
    /// Reliable channel ack.
    pub const ACK: u8 = 0x04;
    /// Video retransmission request.
    pub const NACK: u8 = 0x05;
    pub const PING: u8 = 0x06;
    pub const PONG: u8 = 0x07;
    /// Absolute pointer position, latest wins.
    pub const MOUSE_ABS: u8 = 0x08;
    /// Decoder needs a recovery frame.
    pub const KEYFRAME_REQ: u8 = 0x09;
    /// Receiver statistics report (drives adaptive FEC/bitrate).
    pub const REPORT: u8 = 0x0A;
    pub const KEEPALIVE: u8 = 0x0B;
    pub const DISCONNECT: u8 = 0x0C;
    /// Host cursor position/visibility, latest wins.
    pub const CURSOR_POS: u8 = 0x0D;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    H264 = 1,
    Hevc = 2,
    Av1 = 3,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Codec::H264),
            2 => Some(Codec::Hevc),
            3 => Some(Codec::Av1),
            _ => None,
        }
    }
    pub fn bit(self) -> u8 {
        1 << (self as u8)
    }
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(Codec::H264),
            "hevc" | "h265" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Platform {
    Unknown = 0,
    Linux = 1,
    MacOs = 2,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Unknown
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Platform::Linux,
            2 => Platform::MacOs,
            _ => Platform::Unknown,
        }
    }
}

/// Client capability flags.
pub mod client_flags {
    pub const AUDIO: u32 = 1 << 0;
    /// Client wants the host cursor sent as metadata and drawn locally.
    pub const LOCAL_CURSOR: u32 = 1 << 1;
    /// Client can decode 4:4:4 chroma.
    pub const YUV444: u32 = 1 << 2;
}

/// Sent by the client right after the handshake.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientHello {
    pub platform: Platform,
    /// Bitmask of `Codec::bit()`.
    pub decoders: u8,
    /// Preferred codec (0 = no preference).
    pub codec_pref: u8,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    /// Requested bitrate, 0 = host decides.
    pub bitrate_kbps: u32,
    pub flags: u32,
    /// Index of the host display the client wants (0 = primary / host choice).
    pub display: u8,
}

/// Host → client description of the video stream. Sent before the first
/// frame and whenever the stream changes; a keyframe always follows.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamConfig {
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub flags: u32,
    pub host_platform: Platform,
    pub host_name: String,
}

/// Mouse buttons (wire values).
pub mod button {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 3;
    pub const BACK: u8 = 4;
    pub const FORWARD: u8 = 5;
}

/// Messages carried on the reliable ordered channel.
#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    ClientHello(ClientHello),
    StreamConfig(StreamConfig),
    /// Key press/release; `code` is a Linux evdev key code (see `input`).
    Key { code: u16, down: bool },
    /// Button press/release at a normalized position (0..1 of the video frame).
    MouseButton { button: u8, down: bool, x: f32, y: f32 },
    /// Scroll. If `discrete`, units are wheel notches (fractions allowed for
    /// hi-res wheels); otherwise logical pixels (trackpads).
    Scroll { dx: f32, dy: f32, discrete: bool },
    /// Relative pointer motion in client logical pixels (pointer-lock mode).
    MouseRel { dx: f32, dy: f32 },
    /// Cursor image (RGBA8, premultiplied=false).
    CursorShape { serial: u32, hot_x: u16, hot_y: u16, width: u16, height: u16, rgba: Vec<u8> },
    Clipboard { text: String },
    /// Client asks for a different bitrate (0 = automatic).
    SetBitrate { kbps: u32 },
    /// Release every pressed key/button (focus lost).
    ReleaseAll,
    AudioConfig { sample_rate: u32, channels: u8 },
    /// Free-form status text from the host (e.g. "waiting for screen share approval").
    Status { text: String },
}

mod tag {
    pub const CLIENT_HELLO: u8 = 1;
    pub const STREAM_CONFIG: u8 = 2;
    pub const KEY: u8 = 3;
    pub const MOUSE_BUTTON: u8 = 4;
    pub const SCROLL: u8 = 5;
    pub const MOUSE_REL: u8 = 6;
    pub const CURSOR_SHAPE: u8 = 7;
    pub const CLIPBOARD: u8 = 8;
    pub const SET_BITRATE: u8 = 9;
    pub const RELEASE_ALL: u8 = 10;
    pub const AUDIO_CONFIG: u8 = 11;
    pub const STATUS: u8 = 12;
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32);
        match self {
            Control::ClientHello(h) => {
                b.put_u8(tag::CLIENT_HELLO);
                b.put_u8(h.platform as u8);
                b.put_u8(h.decoders);
                b.put_u8(h.codec_pref);
                b.put_u16(h.max_width);
                b.put_u16(h.max_height);
                b.put_u16(h.max_fps);
                b.put_u32(h.bitrate_kbps);
                b.put_u32(h.flags);
                b.put_u8(h.display);
            }
            Control::StreamConfig(c) => {
                b.put_u8(tag::STREAM_CONFIG);
                b.put_u8(c.codec as u8);
                b.put_u16(c.width);
                b.put_u16(c.height);
                b.put_u16(c.fps);
                b.put_u32(c.bitrate_kbps);
                b.put_u32(c.flags);
                b.put_u8(c.host_platform as u8);
                b.put_str(&c.host_name);
            }
            Control::Key { code, down } => {
                b.put_u8(tag::KEY);
                b.put_u16(*code);
                b.put_u8(*down as u8);
            }
            Control::MouseButton { button, down, x, y } => {
                b.put_u8(tag::MOUSE_BUTTON);
                b.put_u8(*button);
                b.put_u8(*down as u8);
                b.put_f32(*x);
                b.put_f32(*y);
            }
            Control::Scroll { dx, dy, discrete } => {
                b.put_u8(tag::SCROLL);
                b.put_f32(*dx);
                b.put_f32(*dy);
                b.put_u8(*discrete as u8);
            }
            Control::MouseRel { dx, dy } => {
                b.put_u8(tag::MOUSE_REL);
                b.put_f32(*dx);
                b.put_f32(*dy);
            }
            Control::CursorShape { serial, hot_x, hot_y, width, height, rgba } => {
                b.put_u8(tag::CURSOR_SHAPE);
                b.put_u32(*serial);
                b.put_u16(*hot_x);
                b.put_u16(*hot_y);
                b.put_u16(*width);
                b.put_u16(*height);
                b.put_blob32(rgba);
            }
            Control::Clipboard { text } => {
                b.put_u8(tag::CLIPBOARD);
                b.put_blob32(text.as_bytes());
            }
            Control::SetBitrate { kbps } => {
                b.put_u8(tag::SET_BITRATE);
                b.put_u32(*kbps);
            }
            Control::ReleaseAll => b.put_u8(tag::RELEASE_ALL),
            Control::AudioConfig { sample_rate, channels } => {
                b.put_u8(tag::AUDIO_CONFIG);
                b.put_u32(*sample_rate);
                b.put_u8(*channels);
            }
            Control::Status { text } => {
                b.put_u8(tag::STATUS);
                b.put_str(text);
            }
        }
        b
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(match r.u8()? {
            tag::CLIENT_HELLO => Control::ClientHello(ClientHello {
                platform: Platform::from_u8(r.u8()?),
                decoders: r.u8()?,
                codec_pref: r.u8()?,
                max_width: r.u16()?,
                max_height: r.u16()?,
                max_fps: r.u16()?,
                bitrate_kbps: r.u32()?,
                flags: r.u32()?,
                display: r.u8().unwrap_or(0),
            }),
            tag::STREAM_CONFIG => Control::StreamConfig(StreamConfig {
                codec: Codec::from_u8(r.u8()?).ok_or(WireError)?,
                width: r.u16()?,
                height: r.u16()?,
                fps: r.u16()?,
                bitrate_kbps: r.u32()?,
                flags: r.u32()?,
                host_platform: Platform::from_u8(r.u8()?),
                host_name: r.str()?,
            }),
            tag::KEY => Control::Key { code: r.u16()?, down: r.u8()? != 0 },
            tag::MOUSE_BUTTON => Control::MouseButton { button: r.u8()?, down: r.u8()? != 0, x: r.f32()?, y: r.f32()? },
            tag::SCROLL => Control::Scroll { dx: r.f32()?, dy: r.f32()?, discrete: r.u8()? != 0 },
            tag::MOUSE_REL => Control::MouseRel { dx: r.f32()?, dy: r.f32()? },
            tag::CURSOR_SHAPE => Control::CursorShape {
                serial: r.u32()?,
                hot_x: r.u16()?,
                hot_y: r.u16()?,
                width: r.u16()?,
                height: r.u16()?,
                rgba: r.blob32()?.to_vec(),
            },
            tag::CLIPBOARD => Control::Clipboard {
                text: String::from_utf8(r.blob32()?.to_vec()).map_err(|_| WireError)?,
            },
            tag::SET_BITRATE => Control::SetBitrate { kbps: r.u32()? },
            tag::RELEASE_ALL => Control::ReleaseAll,
            tag::AUDIO_CONFIG => Control::AudioConfig { sample_rate: r.u32()?, channels: r.u8()? },
            tag::STATUS => Control::Status { text: r.str()? },
            _ => return Err(WireError),
        })
    }
}

/// Handshake payload sent by the client inside IK message 1.
#[derive(Clone, Debug, PartialEq)]
pub struct HelloPayload {
    pub version: u16,
    /// Wall-clock ns; host rejects replays of older handshakes.
    pub timestamp: u64,
    pub name: String,
}

impl HelloPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_u16(self.version);
        b.put_u64(self.timestamp);
        b.put_str(&self.name);
        b
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { version: r.u16()?, timestamp: r.u64()?, name: r.str()? })
    }
}

/// Pairing payload: static public key + display name.
#[derive(Clone, Debug, PartialEq)]
pub struct PairPayload {
    pub version: u16,
    pub static_key: [u8; 32],
    pub name: String,
}

impl PairPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_u16(self.version);
        b.put_bytes(&self.static_key);
        b.put_str(&self.name);
        b
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { version: r.u16()?, static_key: r.array()?, name: r.str()? })
    }
}

/// Receiver report, sent ~4x per second by the video receiver.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Report {
    /// Video packets received / expected-but-missing in the interval.
    pub packets_received: u32,
    pub packets_lost: u32,
    /// Frames recovered by FEC / by retransmission / lost entirely.
    pub fec_recovered: u32,
    pub retransmit_recovered: u32,
    pub frames_lost: u32,
    pub frames_decoded: u32,
    /// Mean decode+present time on the client (µs).
    pub decode_us: u32,
    /// Mean time from last shard received to frame complete... (network jitter, µs).
    pub jitter_us: u32,
}

impl Report {
    pub fn encode_into(&self, b: &mut Vec<u8>) {
        b.put_u32(self.packets_received);
        b.put_u32(self.packets_lost);
        b.put_u32(self.fec_recovered);
        b.put_u32(self.retransmit_recovered);
        b.put_u32(self.frames_lost);
        b.put_u32(self.frames_decoded);
        b.put_u32(self.decode_us);
        b.put_u32(self.jitter_us);
    }
    pub fn decode(r: &mut Reader) -> Result<Self> {
        Ok(Self {
            packets_received: r.u32()?,
            packets_lost: r.u32()?,
            fec_recovered: r.u32()?,
            retransmit_recovered: r.u32()?,
            frames_lost: r.u32()?,
            frames_decoded: r.u32()?,
            decode_us: r.u32()?,
            jitter_us: r.u32()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let msgs = vec![
            Control::ClientHello(ClientHello {
                platform: Platform::MacOs,
                decoders: Codec::Hevc.bit() | Codec::H264.bit(),
                codec_pref: 2,
                max_width: 3456,
                max_height: 2234,
                max_fps: 120,
                bitrate_kbps: 80_000,
                flags: client_flags::AUDIO,
                display: 1,
            }),
            Control::StreamConfig(StreamConfig {
                codec: Codec::Hevc,
                width: 2560,
                height: 1440,
                fps: 120,
                bitrate_kbps: 60_000,
                flags: 0,
                host_platform: Platform::Linux,
                host_name: "ape-box".into(),
            }),
            Control::Key { code: 30, down: true },
            Control::MouseButton { button: button::RIGHT, down: false, x: 0.25, y: 0.75 },
            Control::Scroll { dx: 0.0, dy: -1.5, discrete: true },
            Control::MouseRel { dx: 3.0, dy: -2.0 },
            Control::CursorShape { serial: 9, hot_x: 1, hot_y: 2, width: 2, height: 1, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] },
            Control::Clipboard { text: "copy ✂".into() },
            Control::SetBitrate { kbps: 1 },
            Control::ReleaseAll,
            Control::AudioConfig { sample_rate: 48000, channels: 2 },
            Control::Status { text: "ok".into() },
        ];
        for m in msgs {
            assert_eq!(Control::decode(&m.encode()).unwrap(), m);
        }
        assert!(Control::decode(&[200]).is_err());
        assert!(Control::decode(&[tag::KEY, 1]).is_err());
    }
}
