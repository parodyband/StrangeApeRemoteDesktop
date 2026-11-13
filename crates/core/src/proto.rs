//! Wire protocol definitions.
//!
//! ```text
//! UDP datagram
//! ├─ 0x01 HANDSHAKE_INIT  [type][sender_idx u32][noise IK msg1]
//! ├─ 0x02 HANDSHAKE_RESP  [type][sender_idx u32][receiver_idx u32][noise IK msg2]
//! ├─ 0x03 DATA            [type][receiver_idx u32][counter u64][AEAD(payload)][tag 16]
//! ├─ 0x04 PAIR_INIT       [type][sender_idx u32][sid 16][noise NNpsk0 msg1, psk = CPace key]
//! ├─ 0x05 PAIR_RESP       [type][sender_idx u32][receiver_idx u32][noise NNpsk0 msg2]
//! ├─ 0x06 PAKE_INIT       [type][sender_idx u32][sid 16][CPace share 32]
//! ├─ 0x07 PAKE_RESP       [type][sender_idx u32][receiver_idx u32][sid 16][CPace share 32]
//! └─ 0x08 PUNCH           [type]  (opens a NAT mapping; ignored on receipt)
//!
//! STUN messages (RFC 8489, magic cookie at bytes 4..8) share the socket and
//! are told apart before the type byte is looked at.
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
    pub const PAKE_INIT: u8 = 0x06;
    pub const PAKE_RESP: u8 = 0x07;
    pub const PUNCH: u8 = 0x08;
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
    Windows = 3,
    Ios = 4,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "ios") {
            Platform::Ios
        } else {
            Platform::Unknown
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Platform::Linux,
            2 => Platform::MacOs,
            3 => Platform::Windows,
            4 => Platform::Ios,
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
    /// Terminal-only session: no video, audio, input or clipboard, just
    /// shells (see `Control::ShellOpen`). Sent in the [`lane::TERMINAL`] lane.
    pub const TERMINAL: u32 = 1 << 3;
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
    /// "Fit to this screen": switch the streamed display to the mode closest
    /// to this desktop size (logical pixels) for the session; 0x0 = leave the
    /// display alone. Trailing fields: older clients omit them.
    pub fit_width: u16,
    pub fit_height: u16,
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
    Key {
        code: u16,
        down: bool,
    },
    /// Button press/release at a normalized position (0..1 of the video frame).
    MouseButton {
        button: u8,
        down: bool,
        x: f32,
        y: f32,
    },
    /// Scroll. If `discrete`, units are wheel notches (fractions allowed for
    /// hi-res wheels); otherwise logical pixels (trackpads).
    Scroll {
        dx: f32,
        dy: f32,
        discrete: bool,
    },
    /// Relative pointer motion in client logical pixels (pointer-lock mode).
    MouseRel {
        dx: f32,
        dy: f32,
    },
    /// Cursor image (RGBA8, premultiplied=false).
    CursorShape {
        serial: u32,
        hot_x: u16,
        hot_y: u16,
        width: u16,
        height: u16,
        rgba: Vec<u8>,
    },
    Clipboard {
        text: String,
    },
    /// Part of a clipboard image (PNG), sent in pieces so a big image never
    /// floods the reliable channel ahead of input. Pieces of one image share
    /// a `serial` and arrive in order; `offset + data.len() == total` ends it.
    /// Newer clipboard contents (text or image) replace an unfinished image.
    ClipboardImage {
        serial: u32,
        total: u32,
        offset: u32,
        data: Vec<u8>,
    },
    /// Client asks for a different bitrate (0 = automatic).
    SetBitrate {
        kbps: u32,
    },
    /// Release every pressed key/button (focus lost).
    ReleaseAll,
    AudioConfig {
        sample_rate: u32,
        channels: u8,
    },
    /// Free-form status text from the host (e.g. "waiting for screen share approval").
    Status {
        text: String,
    },
    /// Host → client: the displays that can be streamed, and which one is.
    Displays {
        current: u32,
        list: Vec<DisplayInfo>,
    },
    /// Client → host: stream another display (`DisplayInfo::id`).
    SelectDisplay {
        id: u32,
    },
    /// Client → host: switch the streamed display to the mode closest to this
    /// desktop size (logical pixels) until the session ends; 0x0 restores it.
    FitDisplay {
        width: u16,
        height: u16,
    },
    /// Client → host: start a shell (`id` 0) or reattach to a running one
    /// (`id` from [`Control::Shells`]). The host answers with
    /// [`Control::ShellOpened`] or [`Control::ShellRefused`] carrying `token`.
    ShellOpen {
        token: u32,
        id: u32,
        cols: u16,
        rows: u16,
    },
    /// Host → client: the shell is running. `resumed`: it was already running
    /// (reattached); its recent output follows.
    ShellOpened {
        token: u32,
        id: u32,
        name: String,
        resumed: bool,
    },
    /// Host → client: no shell (terminal access is off, unknown id, ...).
    ShellRefused {
        token: u32,
        reason: String,
    },
    /// Terminal bytes: client → host is keyboard input, host → client output.
    /// The client acknowledges each output message with [`Control::ShellAck`]
    /// once it has consumed it: the host keeps only a window of
    /// unacknowledged output in flight, so a slow client slows the shell
    /// down instead of overflowing.
    ShellData {
        id: u32,
        data: Vec<u8>,
    },
    /// Client → host: `count` more output messages of shell `id` consumed.
    ShellAck {
        id: u32,
        count: u32,
    },
    ShellResize {
        id: u32,
        cols: u16,
        rows: u16,
    },
    /// Client → host: end the shell (hang up).
    ShellClose {
        id: u32,
    },
    /// Host → client: the shell ended.
    ShellExit {
        id: u32,
        message: String,
    },
    /// Client → host: list this client's shells (answered with `Shells`).
    ShellList,
    /// Host → client: this client's shells, including ones kept running since
    /// an earlier session. Also sent once at the start of a terminal session.
    Shells {
        list: Vec<ShellInfo>,
    },
}

/// A shell kept by the host for one client.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellInfo {
    pub id: u32,
    /// e.g. "zsh"
    pub name: String,
    /// Some session currently shows it.
    pub attached: bool,
    /// Seconds since it started.
    pub age_s: u32,
}

/// A display the host can stream.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    /// Stable within a host session; what `SelectDisplay` refers to.
    pub id: u32,
    /// Human-readable, e.g. "DP-1" or "Built-in Retina Display".
    pub name: String,
    /// Native pixel size.
    pub width: u32,
    pub height: u32,
    /// Position in the host's desktop layout (logical coordinates).
    pub x: i32,
    pub y: i32,
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
    pub const DISPLAYS: u8 = 13;
    pub const SELECT_DISPLAY: u8 = 14;
    pub const FIT_DISPLAY: u8 = 15;
    pub const SHELL_OPEN: u8 = 16;
    pub const SHELL_OPENED: u8 = 17;
    pub const SHELL_REFUSED: u8 = 18;
    pub const SHELL_DATA: u8 = 19;
    pub const SHELL_RESIZE: u8 = 20;
    pub const SHELL_CLOSE: u8 = 21;
    pub const SHELL_EXIT: u8 = 22;
    pub const SHELL_LIST: u8 = 23;
    pub const SHELLS: u8 = 24;
    pub const SHELL_ACK: u8 = 25;
    pub const CLIPBOARD_IMAGE: u8 = 26;
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
                b.put_u16(h.fit_width);
                b.put_u16(h.fit_height);
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
            Control::ClipboardImage { serial, total, offset, data } => {
                b.put_u8(tag::CLIPBOARD_IMAGE);
                b.put_u32(*serial);
                b.put_u32(*total);
                b.put_u32(*offset);
                b.put_blob32(data);
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
            Control::Displays { current, list } => {
                b.put_u8(tag::DISPLAYS);
                b.put_u32(*current);
                b.put_u8(list.len().min(32) as u8);
                for d in list.iter().take(32) {
                    b.put_u32(d.id);
                    b.put_str(&d.name);
                    b.put_u32(d.width);
                    b.put_u32(d.height);
                    b.put_i32(d.x);
                    b.put_i32(d.y);
                }
            }
            Control::SelectDisplay { id } => {
                b.put_u8(tag::SELECT_DISPLAY);
                b.put_u32(*id);
            }
            Control::FitDisplay { width, height } => {
                b.put_u8(tag::FIT_DISPLAY);
                b.put_u16(*width);
                b.put_u16(*height);
            }
            Control::ShellOpen { token, id, cols, rows } => {
                b.put_u8(tag::SHELL_OPEN);
                b.put_u32(*token);
                b.put_u32(*id);
                b.put_u16(*cols);
                b.put_u16(*rows);
            }
            Control::ShellOpened { token, id, name, resumed } => {
                b.put_u8(tag::SHELL_OPENED);
                b.put_u32(*token);
                b.put_u32(*id);
                b.put_str(name);
                b.put_u8(*resumed as u8);
            }
            Control::ShellRefused { token, reason } => {
                b.put_u8(tag::SHELL_REFUSED);
                b.put_u32(*token);
                b.put_str(reason);
            }
            Control::ShellData { id, data } => {
                b.put_u8(tag::SHELL_DATA);
                b.put_u32(*id);
                b.put_blob32(data);
            }
            Control::ShellAck { id, count } => {
                b.put_u8(tag::SHELL_ACK);
                b.put_u32(*id);
                b.put_u32(*count);
            }
            Control::ShellResize { id, cols, rows } => {
                b.put_u8(tag::SHELL_RESIZE);
                b.put_u32(*id);
                b.put_u16(*cols);
                b.put_u16(*rows);
            }
            Control::ShellClose { id } => {
                b.put_u8(tag::SHELL_CLOSE);
                b.put_u32(*id);
            }
            Control::ShellExit { id, message } => {
                b.put_u8(tag::SHELL_EXIT);
                b.put_u32(*id);
                b.put_str(message);
            }
            Control::ShellList => b.put_u8(tag::SHELL_LIST),
            Control::Shells { list } => {
                b.put_u8(tag::SHELLS);
                b.put_u8(list.len().min(64) as u8);
                for i in list.iter().take(64) {
                    b.put_u32(i.id);
                    b.put_str(&i.name);
                    b.put_u8(i.attached as u8);
                    b.put_u32(i.age_s);
                }
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
                fit_width: r.u16().unwrap_or(0),
                fit_height: r.u16().unwrap_or(0),
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
            tag::CLIPBOARD => {
                Control::Clipboard { text: String::from_utf8(r.blob32()?.to_vec()).map_err(|_| WireError)? }
            }
            tag::CLIPBOARD_IMAGE => Control::ClipboardImage {
                serial: r.u32()?,
                total: r.u32()?,
                offset: r.u32()?,
                data: r.blob32()?.to_vec(),
            },
            tag::SET_BITRATE => Control::SetBitrate { kbps: r.u32()? },
            tag::RELEASE_ALL => Control::ReleaseAll,
            tag::AUDIO_CONFIG => Control::AudioConfig { sample_rate: r.u32()?, channels: r.u8()? },
            tag::STATUS => Control::Status { text: r.str()? },
            tag::DISPLAYS => {
                let current = r.u32()?;
                let n = r.u8()? as usize;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    list.push(DisplayInfo {
                        id: r.u32()?,
                        name: r.str()?,
                        width: r.u32()?,
                        height: r.u32()?,
                        x: r.i32()?,
                        y: r.i32()?,
                    });
                }
                Control::Displays { current, list }
            }
            tag::SELECT_DISPLAY => Control::SelectDisplay { id: r.u32()? },
            tag::FIT_DISPLAY => Control::FitDisplay { width: r.u16()?, height: r.u16()? },
            tag::SHELL_OPEN => Control::ShellOpen { token: r.u32()?, id: r.u32()?, cols: r.u16()?, rows: r.u16()? },
            tag::SHELL_OPENED => {
                Control::ShellOpened { token: r.u32()?, id: r.u32()?, name: r.str()?, resumed: r.u8()? != 0 }
            }
            tag::SHELL_REFUSED => Control::ShellRefused { token: r.u32()?, reason: r.str()? },
            tag::SHELL_DATA => Control::ShellData { id: r.u32()?, data: r.blob32()?.to_vec() },
            tag::SHELL_ACK => Control::ShellAck { id: r.u32()?, count: r.u32()? },
            tag::SHELL_RESIZE => Control::ShellResize { id: r.u32()?, cols: r.u16()?, rows: r.u16()? },
            tag::SHELL_CLOSE => Control::ShellClose { id: r.u32()? },
            tag::SHELL_EXIT => Control::ShellExit { id: r.u32()?, message: r.str()? },
            tag::SHELL_LIST => Control::ShellList,
            tag::SHELLS => {
                let n = r.u8()? as usize;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    list.push(ShellInfo { id: r.u32()?, name: r.str()?, attached: r.u8()? != 0, age_s: r.u32()? });
                }
                Control::Shells { list }
            }
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
    /// Which of the client's parallel sessions this is (see [`lane`]). A new
    /// session replaces an older one from the same client only in the same
    /// lane. Trailing field: older peers omit it (= [`lane::SCREEN`]).
    pub lane: u8,
}

/// Session lanes: one client can hold one session per lane at a time.
pub mod lane {
    /// Remote desktop (video, audio, input). Also what older clients use.
    pub const SCREEN: u8 = 0;
    /// Terminal only.
    pub const TERMINAL: u8 = 1;
}

impl HelloPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.put_u16(self.version);
        b.put_u64(self.timestamp);
        b.put_str(&self.name);
        b.put_u8(self.lane);
        b
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { version: r.u16()?, timestamp: r.u64()?, name: r.str()?, lane: r.u8().unwrap_or(lane::SCREEN) })
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

/// Audio packet (payload of `msg::AUDIO`): interleaved s16le stereo, 48 kHz.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioPacket {
    pub seq: u32,
    pub capture_us: u64,
    pub samples: Vec<i16>,
}

impl AudioPacket {
    pub fn encode(seq: u32, capture_us: u64, samples: &[i16]) -> Vec<u8> {
        let mut b = Vec::with_capacity(14 + samples.len() * 2);
        b.put_u32(seq);
        b.put_u64(capture_us);
        b.put_u16((samples.len() / 2) as u16);
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let seq = r.u32()?;
        let capture_us = r.u64()?;
        let frames = r.u16()? as usize;
        let raw = r.bytes(frames * 4)?;
        let samples = raw.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
        Ok(Self { seq, capture_us, samples })
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
                fit_width: 1280,
                fit_height: 590,
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
            Control::CursorShape {
                serial: 9,
                hot_x: 1,
                hot_y: 2,
                width: 2,
                height: 1,
                rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
            },
            Control::Clipboard { text: "copy ✂".into() },
            Control::ClipboardImage { serial: 4, total: 10, offset: 6, data: vec![1, 2, 3, 4] },
            Control::SetBitrate { kbps: 1 },
            Control::ReleaseAll,
            Control::AudioConfig { sample_rate: 48000, channels: 2 },
            Control::Status { text: "ok".into() },
            Control::Displays {
                current: 2,
                list: vec![
                    DisplayInfo { id: 1, name: "DP-1".into(), width: 3840, height: 2160, x: 0, y: 0 },
                    DisplayInfo { id: 2, name: "DP-3".into(), width: 2560, height: 1440, x: 2560, y: 0 },
                ],
            },
            Control::SelectDisplay { id: 2 },
            Control::FitDisplay { width: 1280, height: 590 },
            Control::ShellOpen { token: 7, id: 0, cols: 80, rows: 24 },
            Control::ShellOpened { token: 7, id: 3, name: "zsh".into(), resumed: true },
            Control::ShellRefused { token: 8, reason: "off".into() },
            Control::ShellData { id: 3, data: b"ls -la\r\x1b[A".to_vec() },
            Control::ShellAck { id: 3, count: 2 },
            Control::ShellResize { id: 3, cols: 120, rows: 40 },
            Control::ShellClose { id: 3 },
            Control::ShellExit { id: 3, message: "exited with status 0".into() },
            Control::ShellList,
            Control::Shells { list: vec![ShellInfo { id: 3, name: "zsh".into(), attached: false, age_s: 600 }] },
        ];
        for m in msgs {
            assert_eq!(Control::decode(&m.encode()).unwrap(), m);
        }
        assert!(Control::decode(&[200]).is_err());
        let a = AudioPacket::encode(7, 99, &[1, -2, 3, -4]);
        assert_eq!(
            AudioPacket::decode(&a).unwrap(),
            AudioPacket { seq: 7, capture_us: 99, samples: vec![1, -2, 3, -4] }
        );
        assert!(Control::decode(&[tag::KEY, 1]).is_err());
    }

    /// Trailing fields added later decode from older peers' shorter messages.
    #[test]
    fn older_peers_messages_still_decode() {
        let hello = ClientHello {
            platform: Platform::Linux,
            decoders: Codec::Hevc.bit(),
            codec_pref: 0,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            bitrate_kbps: 0,
            flags: 0,
            display: 2,
            fit_width: 0,
            fit_height: 0,
        };
        let mut old = Control::ClientHello(hello.clone()).encode();
        old.truncate(old.len() - 4); // no fit size
        assert_eq!(Control::decode(&old).unwrap(), Control::ClientHello(hello));
        let h = HelloPayload { version: 1, timestamp: 5, name: "old".into(), lane: lane::SCREEN };
        let mut old = HelloPayload { lane: lane::TERMINAL, ..h.clone() }.encode();
        old.pop(); // no lane
        assert_eq!(HelloPayload::decode(&old).unwrap(), h);
    }
}
