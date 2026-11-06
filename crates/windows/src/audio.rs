//! WASAPI audio, shared mode: playback of the host's audio (viewer) and
//! loopback capture of what this machine plays (host), as 48 kHz stereo s16.
//!
//! Each stream runs on its own thread with the "Pro Audio" MMCSS priority.
//! Playback asks Windows to convert our format to the device's
//! (`AUTOCONVERTPCM`); capture takes the device's mix format (usually 32-bit
//! float) and converts / resamples itself.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, clippy::upper_case_acronyms, dead_code)]

use crate::com::{self, Interface};
use crate::com_interface;
use crate::sys::{self, GUID, HANDLE, HRESULT};
use sard_core::media::{self, AUDIO_CHANNELS, AUDIO_RATE, AudioCallback, AudioSink, AudioSource, JitterBuffer};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

com_interface! {
    pub struct IMMDeviceEnumerator(0xa95664d2_9614_4f35_a746_de8db63617e6);
    4 fn GetDefaultAudioEndpoint(flow: u32, role: u32, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IMMDevice(0xd666063f_1587_4e43_81f1_b948e807363f);
    3 fn Activate(iid: *const GUID, ctx: u32, params: *mut c_void, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IAudioClient(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
    3 fn Initialize(mode: u32, flags: u32, duration: i64, period: i64, format: *const c_void, session: *const GUID) -> HRESULT;
    4 fn GetBufferSize(frames: *mut u32) -> HRESULT;
    6 fn GetCurrentPadding(frames: *mut u32) -> HRESULT;
    8 fn GetMixFormat(format: *mut *mut c_void) -> HRESULT;
    10 fn Start() -> HRESULT;
    11 fn Stop() -> HRESULT;
    13 fn SetEventHandle(event: HANDLE) -> HRESULT;
    14 fn GetService(iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
}

com_interface! {
    pub struct IAudioRenderClient(0xf294acfc_3146_4483_a7bf_addca7c260e2);
    3 fn GetBuffer(frames: u32, data: *mut *mut u8) -> HRESULT;
    4 fn ReleaseBuffer(frames: u32, flags: u32) -> HRESULT;
}

com_interface! {
    pub struct IAudioCaptureClient(0xc8adbd64_e71e_48a0_a4de_185c395cd317);
    3 fn GetBuffer(data: *mut *mut u8, frames: *mut u32, flags: *mut u32, pos: *mut u64, qpc: *mut u64) -> HRESULT;
    4 fn ReleaseBuffer(frames: u32) -> HRESULT;
    5 fn GetNextPacketSize(frames: *mut u32) -> HRESULT;
}

pub const CLSID_MMDeviceEnumerator: GUID = GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);
pub const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

pub const eRender: u32 = 0;
pub const eConsole: u32 = 0;
pub const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
pub const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
pub const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
pub const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
pub const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
pub const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `WAVEFORMATEX` (packed: 18 bytes).
#[repr(C, packed(2))]
#[derive(Clone, Copy, Default)]
pub struct WAVEFORMATEX {
    pub wFormatTag: u16,
    pub nChannels: u16,
    pub nSamplesPerSec: u32,
    pub nAvgBytesPerSec: u32,
    pub nBlockAlign: u16,
    pub wBitsPerSample: u16,
    pub cbSize: u16,
}

const _: () = assert!(std::mem::size_of::<WAVEFORMATEX>() == 18);

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> HANDLE;
}

fn pro_audio_priority() {
    let task = sys::wide("Pro Audio");
    let mut idx = 0u32;
    // SAFETY: valid string and out pointer; the handle lives as long as the thread.
    unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx) };
}

fn default_render_device() -> Result<IMMDevice, String> {
    let e: IMMDeviceEnumerator = com::out(
        |p| unsafe {
            sys::CoCreateInstance(
                &CLSID_MMDeviceEnumerator,
                std::ptr::null_mut(),
                sys::CLSCTX_ALL,
                &IMMDeviceEnumerator::IID,
                p,
            )
        },
        "CoCreateInstance(MMDeviceEnumerator)",
    )?;
    com::out(|p| unsafe { e.GetDefaultAudioEndpoint(eRender, eConsole, p) }, "no default audio output device")
}

fn activate_client(dev: &IMMDevice) -> Result<IAudioClient, String> {
    com::out(
        |p| unsafe { dev.Activate(&IAudioClient::IID, sys::CLSCTX_ALL, std::ptr::null_mut(), p) },
        "IMMDevice::Activate(IAudioClient)",
    )
}

struct Event(HANDLE);

// SAFETY: event handles may be used from any thread.
unsafe impl Send for Event {}

impl Event {
    fn new() -> Result<Event, String> {
        // SAFETY: plain auto-reset event.
        let h = unsafe { sys::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if h.is_null() { Err("CreateEvent failed".into()) } else { Ok(Event(h)) }
    }
    fn wait(&self, ms: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::WaitForSingleObject(self.0, ms) == sys::WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: our handle.
        unsafe { sys::CloseHandle(self.0) };
    }
}

/// Runs `body` on a new COM thread; returns once it reports successful setup.
fn spawn_stream(
    name: &str,
    body: impl FnOnce(mpsc::Sender<Result<String, String>>) + Send + 'static,
) -> media::Result<(std::thread::JoinHandle<()>, String)> {
    let (tx, rx) = mpsc::channel();
    let t = std::thread::Builder::new().name(name.into()).spawn(move || {
        sys::com_init();
        pro_audio_priority();
        body(tx);
    })?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(desc)) => Ok((t, desc)),
        Ok(Err(e)) => Err(media::err(e)),
        Err(_) => Err(media::err("audio device did not start")),
    }
}

// ---------------------------------------------------------------------------
// Playback

pub struct WasapiSink {
    jitter: Arc<Mutex<JitterBuffer>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    desc: String,
}

impl WasapiSink {
    pub fn new() -> media::Result<WasapiSink> {
        let jitter = Arc::new(Mutex::new(JitterBuffer::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (j, s) = (Arc::clone(&jitter), Arc::clone(&stop));
        let (thread, desc) = spawn_stream("sard-audio-out", move |ready| {
            if let Err(e) = render_loop(&j, &s, &ready) {
                let _ = ready.send(Err(e.clone()));
                sard_core::warn!("audio playback stopped: {e}");
            }
        })?;
        Ok(WasapiSink { jitter, stop, thread: Some(thread), desc })
    }
}

fn render_loop(
    jitter: &Mutex<JitterBuffer>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<String, String>>,
) -> Result<(), String> {
    let dev = default_render_device()?;
    let client = activate_client(&dev)?;
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: AUDIO_CHANNELS as u16,
        nSamplesPerSec: AUDIO_RATE,
        nAvgBytesPerSec: AUDIO_RATE * AUDIO_CHANNELS as u32 * 2,
        nBlockAlign: AUDIO_CHANNELS as u16 * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // 20 ms device buffer (in 100 ns units).
    // SAFETY: valid format; null session = default.
    let hr = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            200_000,
            0,
            (&fmt as *const WAVEFORMATEX).cast(),
            std::ptr::null(),
        )
    };
    sys::check(hr, "IAudioClient::Initialize (playback)")?;
    let event = Event::new()?;
    // SAFETY: valid event handle.
    sys::check(unsafe { client.SetEventHandle(event.0) }, "SetEventHandle")?;
    let render: IAudioRenderClient =
        com::out(|p| unsafe { client.GetService(&IAudioRenderClient::IID, p) }, "GetService(IAudioRenderClient)")?;
    let mut size = 0u32;
    // SAFETY: valid out pointer.
    sys::check(unsafe { client.GetBufferSize(&mut size) }, "GetBufferSize")?;
    // SAFETY: plain call.
    sys::check(unsafe { client.Start() }, "IAudioClient::Start")?;
    let _ = ready.send(Ok(format!("WASAPI default output ({} ms buffer)", size * 1000 / AUDIO_RATE)));
    let mut scratch = vec![0i16; size as usize * AUDIO_CHANNELS];
    while !stop.load(Ordering::Relaxed) {
        event.wait(50);
        let mut padding = 0u32;
        // SAFETY: valid out pointer.
        if unsafe { client.GetCurrentPadding(&mut padding) } < 0 {
            break; // device gone (unplugged / default changed)
        }
        let frames = size.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        let n = frames as usize * AUDIO_CHANNELS;
        jitter.lock().unwrap().pull(&mut scratch[..n]);
        let mut data = std::ptr::null_mut();
        // SAFETY: GetBuffer gives room for `frames` frames of our format until ReleaseBuffer.
        unsafe {
            if render.GetBuffer(frames, &mut data) < 0 {
                break;
            }
            std::ptr::copy_nonoverlapping(scratch.as_ptr() as *const u8, data, n * 2);
            render.ReleaseBuffer(frames, 0);
        }
    }
    // SAFETY: plain call.
    unsafe { client.Stop() };
    Ok(())
}

impl AudioSink for WasapiSink {
    fn push(&self, seq: u32, samples: &[i16]) {
        self.jitter.lock().unwrap().push(seq, samples);
    }

    fn describe(&self) -> String {
        self.desc.clone()
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Loopback capture

pub struct LoopbackSource {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    desc: String,
}

impl LoopbackSource {
    pub fn new(cb: AudioCallback) -> media::Result<LoopbackSource> {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let (thread, desc) = spawn_stream("sard-audio-in", move |ready| {
            if let Err(e) = capture_loop(cb, &s, &ready) {
                let _ = ready.send(Err(e.clone()));
                sard_core::warn!("audio capture stopped: {e}");
            }
        })?;
        Ok(LoopbackSource { stop, thread: Some(thread), desc })
    }
}

/// The device's sample format.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleFormat {
    F32,
    I16,
    I32,
    I24,
}

fn mix_format(p: *const u8) -> Result<(SampleFormat, usize, u32, usize), String> {
    // SAFETY: GetMixFormat returned a WAVEFORMATEX (or WAVEFORMATEXTENSIBLE: 40 bytes when cbSize >= 22).
    unsafe {
        let w = (p as *const WAVEFORMATEX).read_unaligned();
        let tag = if w.wFormatTag == WAVE_FORMAT_EXTENSIBLE && w.cbSize >= 22 {
            let sub = (p.add(24) as *const GUID).read_unaligned();
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                WAVE_FORMAT_IEEE_FLOAT
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                WAVE_FORMAT_PCM
            } else {
                return Err(format!("unsupported mix format subtype {sub}"));
            }
        } else {
            w.wFormatTag
        };
        let fmt = match (tag, w.wBitsPerSample) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFormat::F32,
            (WAVE_FORMAT_PCM, 16) => SampleFormat::I16,
            (WAVE_FORMAT_PCM, 32) => SampleFormat::I32,
            (WAVE_FORMAT_PCM, 24) => SampleFormat::I24,
            (t, b) => return Err(format!("unsupported mix format (tag {t}, {b} bits)")),
        };
        Ok((fmt, w.nChannels.max(1) as usize, w.nSamplesPerSec, w.nBlockAlign as usize))
    }
}

/// Converts device frames to 48 kHz stereo s16 (linear resampling).
struct Converter {
    fmt: SampleFormat,
    channels: usize,
    block: usize,
    step: f64,
    pos: f64,
    prev: [f32; 2],
    out: Vec<i16>,
}

impl Converter {
    fn new(fmt: SampleFormat, channels: usize, rate: u32, block: usize) -> Self {
        Converter {
            fmt,
            channels,
            block,
            step: rate as f64 / AUDIO_RATE as f64,
            pos: 0.0,
            prev: [0.0; 2],
            out: Vec::new(),
        }
    }

    fn sample(&self, frame: &[u8], ch: usize) -> f32 {
        let ch = ch.min(self.channels - 1);
        match self.fmt {
            SampleFormat::F32 => f32::from_le_bytes(frame[ch * 4..ch * 4 + 4].try_into().unwrap()),
            SampleFormat::I16 => i16::from_le_bytes(frame[ch * 2..ch * 2 + 2].try_into().unwrap()) as f32 / 32768.0,
            SampleFormat::I32 => {
                i32::from_le_bytes(frame[ch * 4..ch * 4 + 4].try_into().unwrap()) as f32 / 2_147_483_648.0
            }
            SampleFormat::I24 => {
                let b = &frame[ch * 3..ch * 3 + 3];
                (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
            }
        }
    }

    /// Feed `n` frames (`None`: silence); returns the converted samples.
    fn feed(&mut self, data: Option<&[u8]>, n: usize) -> &[i16] {
        self.out.clear();
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        for i in 0..n {
            let cur = match data {
                Some(d) => {
                    let f = &d[i * self.block..(i + 1) * self.block];
                    [self.sample(f, 0), self.sample(f, 1)]
                }
                None => [0.0; 2],
            };
            if self.step == 1.0 {
                self.out.push(to_i16(cur[0]));
                self.out.push(to_i16(cur[1]));
            } else {
                // Output samples falling between the previous input frame and this one.
                while self.pos <= 1.0 {
                    let t = self.pos as f32;
                    self.out.push(to_i16(self.prev[0] + (cur[0] - self.prev[0]) * t));
                    self.out.push(to_i16(self.prev[1] + (cur[1] - self.prev[1]) * t));
                    self.pos += self.step;
                }
                self.pos -= 1.0;
            }
            self.prev = cur;
        }
        &self.out
    }
}

fn capture_loop(
    mut cb: AudioCallback,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<String, String>>,
) -> Result<(), String> {
    let dev = default_render_device()?;
    let client = activate_client(&dev)?;
    let mut mix = std::ptr::null_mut();
    // SAFETY: valid out pointer; freed with CoTaskMemFree below.
    sys::check(unsafe { client.GetMixFormat(&mut mix) }, "GetMixFormat")?;
    let parsed = mix_format(mix as *const u8);
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    // SAFETY: the mix format the device gave us.
    let hr = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, mix, std::ptr::null()) };
    // SAFETY: allocated by GetMixFormat.
    unsafe { sys::CoTaskMemFree(mix) };
    let (fmt, channels, rate, block) = parsed?;
    sys::check(hr, "IAudioClient::Initialize (loopback)")?;
    let event = Event::new()?;
    // SAFETY: valid event handle.
    sys::check(unsafe { client.SetEventHandle(event.0) }, "SetEventHandle")?;
    let cap: IAudioCaptureClient =
        com::out(|p| unsafe { client.GetService(&IAudioCaptureClient::IID, p) }, "GetService(IAudioCaptureClient)")?;
    // SAFETY: plain call.
    sys::check(unsafe { client.Start() }, "IAudioClient::Start")?;
    let _ = ready.send(Ok(format!("WASAPI loopback of the default output ({rate} Hz, {channels} ch, {fmt:?})")));
    let mut conv = Converter::new(fmt, channels, rate, block);
    while !stop.load(Ordering::Relaxed) {
        // Loopback can stay silent (nothing playing): poll anyway.
        event.wait(20);
        loop {
            let mut next = 0u32;
            // SAFETY: valid out pointer.
            if unsafe { cap.GetNextPacketSize(&mut next) } < 0 {
                return Err("audio device lost".into());
            }
            if next == 0 {
                break;
            }
            let (mut data, mut frames, mut bflags) = (std::ptr::null_mut(), 0u32, 0u32);
            // SAFETY: GetBuffer/ReleaseBuffer pair; `frames * block` bytes are readable meanwhile.
            unsafe {
                if cap.GetBuffer(&mut data, &mut frames, &mut bflags, std::ptr::null_mut(), std::ptr::null_mut()) < 0 {
                    break;
                }
                let n = frames as usize;
                let src = if bflags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
                    None
                } else {
                    Some(std::slice::from_raw_parts(data, n * block))
                };
                let out = conv.feed(src, n);
                if !out.is_empty() {
                    cb(out);
                }
                cap.ReleaseBuffer(frames);
            }
        }
    }
    // SAFETY: plain call.
    unsafe { client.Stop() };
    Ok(())
}

impl AudioSource for LoopbackSource {
    fn describe(&self) -> String {
        self.desc.clone()
    }
}

impl Drop for LoopbackSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_resamples_44k1_to_48k() {
        let mut c = Converter::new(SampleFormat::F32, 2, 44_100, 8);
        let frames: Vec<u8> = (0..441).flat_map(|_| [0.5f32.to_le_bytes(), (-0.5f32).to_le_bytes()].concat()).collect();
        let n = c.feed(Some(&frames), 441).len() / 2;
        assert!((479..=481).contains(&n), "{n} frames");
        let mut c = Converter::new(SampleFormat::I16, 1, 48_000, 2);
        let out = c.feed(Some(&[0x00, 0x40]), 1).to_vec();
        assert_eq!(out, vec![16384, 16384]);
    }
}
