//! Audio output via a default-output AudioUnit (CoreAudio HAL), pulling from
//! the shared jitter buffer in its render callback. 48 kHz stereo s16.

use sard_core::media::{self, AUDIO_CHANNELS, AUDIO_RATE, AudioSink, JitterBuffer};
use std::ffi::c_void;
use std::sync::Mutex;

type OSStatus = i32;
type AudioUnit = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type RenderProc =
    unsafe extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> OSStatus;

#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: RenderProc,
    input_proc_ref_con: *mut c_void,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(component: *mut c_void, desc: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        size: u32,
    ) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = fourcc(b"auou");
const K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;

pub struct MacAudioSink {
    unit: AudioUnit,
    jitter: Box<Mutex<JitterBuffer>>,
}

// SAFETY: the AudioUnit is driven by CoreAudio's thread; shared state is locked.
unsafe impl Send for MacAudioSink {}

unsafe extern "C" fn render(
    ref_con: *mut c_void,
    _flags: *mut u32,
    _ts: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: CoreAudio passes our ref_con and a valid buffer list.
    unsafe {
        let jitter = &*(ref_con as *const Mutex<JitterBuffer>);
        let buf = &mut (*data).buffers[0];
        let n = (frames as usize * AUDIO_CHANNELS).min(buf.data_byte_size as usize / 2);
        let out = std::slice::from_raw_parts_mut(buf.data as *mut i16, n);
        match jitter.try_lock() {
            Ok(mut j) => j.pull(out),
            Err(_) => out.fill(0),
        }
    }
    0
}

fn check(r: OSStatus, what: &str) -> media::Result<()> {
    if r == 0 { Ok(()) } else { Err(media::err(format!("{what} failed (OSStatus {r})"))) }
}

impl MacAudioSink {
    pub fn new() -> media::Result<Self> {
        let desc = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT,
            component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        // SAFETY: standard AudioUnit setup; the unit is disposed in Drop.
        unsafe {
            let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
            if comp.is_null() {
                return Err(media::err("no default audio output"));
            }
            let mut unit: AudioUnit = std::ptr::null_mut();
            check(AudioComponentInstanceNew(comp, &mut unit), "AudioComponentInstanceNew")?;
            let me = MacAudioSink { unit, jitter: Box::new(Mutex::new(JitterBuffer::default())) };
            let fmt = AudioStreamBasicDescription {
                sample_rate: AUDIO_RATE as f64,
                format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
                bytes_per_packet: 4,
                frames_per_packet: 1,
                bytes_per_frame: 4,
                channels_per_frame: AUDIO_CHANNELS as u32,
                bits_per_channel: 16,
                reserved: 0,
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    (&fmt as *const AudioStreamBasicDescription).cast(),
                    std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                ),
                "set stream format",
            )?;
            let cb = AURenderCallbackStruct {
                input_proc: render,
                input_proc_ref_con: &*me.jitter as *const Mutex<JitterBuffer> as *mut c_void,
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    (&cb as *const AURenderCallbackStruct).cast(),
                    std::mem::size_of::<AURenderCallbackStruct>() as u32,
                ),
                "set render callback",
            )?;
            // Small hardware buffer (~5 ms) for low latency; best effort.
            let frames: u32 = 256;
            AudioUnitSetProperty(
                unit,
                K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                (&frames as *const u32).cast(),
                4,
            );
            check(AudioUnitInitialize(unit), "AudioUnitInitialize")?;
            check(AudioOutputUnitStart(unit), "AudioOutputUnitStart")?;
            Ok(me)
        }
    }

    #[cfg(test)]
    fn buffered_frames(&self) -> usize {
        self.jitter.lock().unwrap().frames()
    }
}

impl AudioSink for MacAudioSink {
    fn push(&self, seq: u32, samples: &[i16]) {
        self.jitter.lock().unwrap().push(seq, samples);
    }

    fn describe(&self) -> String {
        "CoreAudio default output".into()
    }
}

impl Drop for MacAudioSink {
    fn drop(&mut self) {
        // SAFETY: tear down the unit we created; the callback stops before dispose.
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sard_core::media::AUDIO_PACKET_FRAMES;

    /// Feeds silence in real time (inaudible); CoreAudio must drain it.
    #[test]
    fn output_unit_consumes_audio() {
        let Ok(sink) = MacAudioSink::new() else { return };
        let silence = vec![0i16; AUDIO_PACKET_FRAMES * 2];
        for seq in 0..200u32 {
            sink.push(seq, &silence);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let frames = sink.buffered_frames();
        assert!(frames <= JitterBuffer::default().max_frames, "not drained: {frames}");
        eprintln!("coreaudio: {frames} frames buffered after 1 s");
    }
}
