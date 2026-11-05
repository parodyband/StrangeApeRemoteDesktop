//! PipeWire audio: capture of the default output ("what you hear") on the
//! host, and low-latency playback on the client. 48 kHz stereo s16, 5 ms quantum.

use super::layout as L;
use super::pod::{Pod, array, id, int, object, prop, u32s};
use super::{PwApi, api};
use sard_core::media::{
    self, AUDIO_CHANNELS, AUDIO_PACKET_FRAMES, AUDIO_RATE, AudioCallback, AudioSink, AudioSource, JitterBuffer,
};
use std::ffi::{CString, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

enum Mode {
    Capture(Mutex<AudioCallback>),
    Playback(Mutex<JitterBuffer>),
}

struct Inner {
    api: &'static PwApi,
    stream: AtomicPtr<c_void>,
    mode: Mode,
}

pub struct AudioStream {
    inner: Box<Inner>,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    _events: Box<[u64; L::STREAM_EVENTS_SIZE / 8]>,
    _hook: Box<[u64; L::SPA_HOOK_SIZE / 8]>,
    what: &'static str,
}

// SAFETY: the stream is driven by its own loop thread; shared state is locked.
unsafe impl Send for AudioStream {}

unsafe fn rd_ptr(p: *const u8, off: usize) -> *mut u8 {
    unsafe { (p.add(off) as *const *mut u8).read_unaligned() }
}
unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    unsafe { (p.add(off) as *const u32).read_unaligned() }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let inner = unsafe { &*(data as *const Inner) };
    let api = inner.api;
    let stream = inner.stream.load(Ordering::Relaxed);
    // SAFETY: on the loop thread with a valid stream.
    unsafe {
        let b = (api.pw_stream_dequeue_buffer)(stream);
        if b.is_null() {
            return;
        }
        let spa = rd_ptr(b as *const u8, L::PWBUF_BUFFER);
        let d0 = rd_ptr(spa, L::BUF_DATAS);
        let ptr = rd_ptr(d0, L::DATA_DATA);
        let chunk = rd_ptr(d0, L::DATA_CHUNK);
        if !ptr.is_null() {
            match &inner.mode {
                Mode::Capture(cb) => {
                    let off = rd_u32(chunk, L::CHUNK_OFFSET) as usize;
                    let size = rd_u32(chunk, L::CHUNK_SIZE) as usize;
                    let samples = std::slice::from_raw_parts(ptr.add(off) as *const i16, size / 2);
                    (cb.lock().unwrap())(samples);
                }
                Mode::Playback(jb) => {
                    let max = rd_u32(d0, L::DATA_MAXSIZE) as usize / 4;
                    let req = (b as *const u8).add(L::PWBUF_REQUESTED).cast::<u64>().read_unaligned() as usize;
                    let frames = if req > 0 { req.min(max) } else { max.min(AUDIO_PACKET_FRAMES) };
                    let out = std::slice::from_raw_parts_mut(ptr as *mut i16, frames * AUDIO_CHANNELS);
                    jb.lock().unwrap().pull(out);
                    (chunk.add(L::CHUNK_OFFSET) as *mut u32).write_unaligned(0);
                    (chunk.add(L::CHUNK_STRIDE) as *mut i32).write_unaligned(4);
                    (chunk.add(L::CHUNK_SIZE) as *mut u32).write_unaligned((frames * 4) as u32);
                }
            }
        }
        (api.pw_stream_queue_buffer)(stream, b);
    }
}

fn format_pod() -> Vec<u8> {
    object(
        L::OBJ_FORMAT,
        L::PARAM_ENUM_FORMAT,
        &[
            prop(L::FORMAT_MEDIA_TYPE, 0, id(L::MEDIA_TYPE_AUDIO)),
            prop(L::FORMAT_MEDIA_SUBTYPE, 0, id(L::MEDIA_SUBTYPE_RAW)),
            prop(L::FORMAT_AUDIO_FORMAT, 0, id(L::AUDIO_FORMAT_S16_LE)),
            prop(L::FORMAT_AUDIO_RATE, 0, int(AUDIO_RATE as i32)),
            prop(L::FORMAT_AUDIO_CHANNELS, 0, int(AUDIO_CHANNELS as i32)),
            prop(L::FORMAT_AUDIO_POSITION, 0, array(L::T_ID, &[u32s(L::AUDIO_CHANNEL_FL), u32s(L::AUDIO_CHANNEL_FR)])),
        ],
    )
}

impl AudioStream {
    fn open(mode: Mode, playback: bool) -> Result<Self, String> {
        let api = api()?;
        let what = if playback { "playback" } else { "capture" };
        // SAFETY: same setup/teardown discipline as the video stream.
        unsafe {
            let name = CString::new(format!("sard-audio-{what}")).unwrap();
            let tl = (api.pw_thread_loop_new)(name.as_ptr(), std::ptr::null());
            let context = (api.pw_context_new)((api.pw_thread_loop_get_loop)(tl), std::ptr::null_mut(), 0);
            if tl.is_null() || context.is_null() {
                return Err("pipewire audio: loop/context creation failed".into());
            }
            (api.pw_thread_loop_start)(tl);
            (api.pw_thread_loop_lock)(tl);
            let mut s = AudioStream {
                inner: Box::new(Inner { api, stream: AtomicPtr::new(std::ptr::null_mut()), mode }),
                thread_loop: tl,
                context,
                core: std::ptr::null_mut(),
                _events: Box::new([0; L::STREAM_EVENTS_SIZE / 8]),
                _hook: Box::new([0; L::SPA_HOOK_SIZE / 8]),
                what,
            };
            let r = s.setup(playback);
            (api.pw_thread_loop_unlock)(tl);
            r?;
            Ok(s)
        }
    }

    unsafe fn setup(&mut self, playback: bool) -> Result<(), String> {
        let api = self.inner.api;
        unsafe {
            self.core = (api.pw_context_connect)(self.context, std::ptr::null_mut(), 0);
            if self.core.is_null() {
                return Err("cannot connect to the PipeWire daemon".into());
            }
            let props = if playback {
                format!(
                    "media.type=Audio media.category=Playback media.role=Game node.latency={AUDIO_PACKET_FRAMES}/{AUDIO_RATE} node.name=sard-playback node.description=\"sard remote audio\""
                )
            } else {
                format!(
                    "media.type=Audio media.category=Capture media.role=Music stream.capture.sink=true node.latency={AUDIO_PACKET_FRAMES}/{AUDIO_RATE} node.name=sard-capture node.description=\"sard audio capture\" node.dont-reconnect=false"
                )
            };
            let props = CString::new(props).unwrap();
            let name = CString::new(format!("sard-{}", self.what)).unwrap();
            let stream = (api.pw_stream_new)(self.core, name.as_ptr(), (api.pw_properties_new_string)(props.as_ptr()));
            if stream.is_null() {
                return Err("pw_stream_new (audio) failed".into());
            }
            self.inner.stream.store(stream, Ordering::Relaxed);
            let ev = self._events.as_mut_ptr() as *mut u8;
            (ev.add(L::EV_VERSION) as *mut u32).write(L::VERSION_STREAM_EVENTS);
            (ev.add(L::EV_PROCESS) as *mut usize).write(on_process as *const () as usize);
            let data = &*self.inner as *const Inner as *mut c_void;
            (api.pw_stream_add_listener)(stream, self._hook.as_mut_ptr().cast(), self._events.as_ptr().cast(), data);
            let pod = Pod::from_bytes(&format_pod());
            let mut ptrs = [pod.as_ptr()];
            let dir = if playback { L::DIRECTION_OUTPUT } else { L::DIRECTION_INPUT };
            let r = (api.pw_stream_connect)(
                stream,
                dir,
                L::ID_ANY,
                L::FLAG_AUTOCONNECT | L::FLAG_MAP_BUFFERS | L::FLAG_RT_PROCESS,
                ptrs.as_mut_ptr(),
                1,
            );
            if r < 0 {
                return Err(format!("pw_stream_connect (audio {}) failed ({r})", self.what));
            }
        }
        Ok(())
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        let api = self.inner.api;
        // SAFETY: orderly teardown with the loop locked, then stopped.
        unsafe {
            (api.pw_thread_loop_lock)(self.thread_loop);
            let stream = self.inner.stream.swap(std::ptr::null_mut(), Ordering::Relaxed);
            if !stream.is_null() {
                (api.pw_stream_disconnect)(stream);
                (api.pw_stream_destroy)(stream);
            }
            if !self.core.is_null() {
                (api.pw_core_disconnect)(self.core);
            }
            (api.pw_thread_loop_unlock)(self.thread_loop);
            (api.pw_thread_loop_stop)(self.thread_loop);
            (api.pw_context_destroy)(self.context);
            (api.pw_thread_loop_destroy)(self.thread_loop);
        }
    }
}

impl AudioSource for AudioStream {
    fn describe(&self) -> String {
        "PipeWire monitor of the default output".into()
    }
}

impl AudioSink for AudioStream {
    fn push(&self, seq: u32, samples: &[i16]) {
        if let Mode::Playback(jb) = &self.inner.mode {
            jb.lock().unwrap().push(seq, samples);
        }
    }
    fn describe(&self) -> String {
        "PipeWire playback".into()
    }
}

/// Capture what the default output device plays.
pub fn open_source(cb: AudioCallback) -> media::Result<Box<dyn AudioSource>> {
    Ok(Box::new(AudioStream::open(Mode::Capture(Mutex::new(cb)), false)?))
}

pub fn open_sink() -> media::Result<Box<dyn AudioSink>> {
    Ok(Box::new(AudioStream::open(Mode::Playback(Mutex::new(JitterBuffer::default())), true)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a playback stream and feeds it silence in real time (inaudible).
    /// Skips when no PipeWire daemon is reachable.
    #[test]
    fn playback_stream_consumes_audio() {
        let Ok(stream) = AudioStream::open(Mode::Playback(Mutex::new(JitterBuffer::default())), true) else { return };
        let silence = vec![0i16; AUDIO_PACKET_FRAMES * 2];
        for seq in 0..200u32 {
            stream.push(seq, &silence);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let Mode::Playback(jb) = &stream.inner.mode else { unreachable!() };
        let jb = jb.lock().unwrap();
        // If PipeWire is pulling, the buffer stays bounded near the target.
        assert!(jb.frames() <= jb.max_frames, "buffer not drained: {} frames", jb.frames());
        eprintln!(
            "playback: {} frames buffered, {} underruns, {} dropped",
            jb.frames(),
            jb.underruns,
            jb.dropped_frames
        );
    }
}
