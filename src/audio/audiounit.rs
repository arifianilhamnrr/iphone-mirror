// language: Rust, file: src/audio/audiounit.rs
#![allow(non_upper_case_globals)]
//! Default-output AudioUnit backend. CoreAudio pulls PCM from the ring buffer
//! through a real-time render callback, so the hardware clock drives playback.

use super::{
    output::{AudioOutput, AudioOutputStats, AudioRestartReason, MuteState},
    ring_buffer::{PcmRing, CHANNELS},
};
use anyhow::{bail, Result};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

// ---- CoreAudio FFI -------------------------------------------------------

const SAMPLE_RATE: f64 = 48_000.0;

// FourCC helpers.
const fn fourcc(value: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*value)
}

const kAudioUnitType_Output: u32 = fourcc(b"auou");
const kAudioUnitSubType_DefaultOutput: u32 = fourcc(b"def ");
const kAudioUnitManufacturer_Apple: u32 = fourcc(b"appl");
const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");

const kAudioUnitProperty_StreamFormat: u32 = 8;
const kAudioUnitProperty_SetRenderCallback: u32 = 23;
const kAudioUnitScope_Input: u32 = 1;
const kAudioObjectSystemObject: u32 = 1;
const kAudioHardwarePropertyDefaultOutputDevice: u32 = fourcc(b"dOut");
const kAudioObjectPropertyScopeGlobal: u32 = 0;
const kAudioObjectPropertyElementMain: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
struct AudioBuffer {
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AURenderCallbackStruct {
    input_proc: Option<
        extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> i32,
    >,
    input_proc_ref_con: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

type AudioComponent = *mut c_void;
type AudioComponentInstance = *mut c_void;

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(
        component: AudioComponent,
        desc: *const AudioComponentDescription,
    ) -> AudioComponent;
    fn AudioComponentInstanceNew(
        component: AudioComponent,
        instance: *mut AudioComponentInstance,
    ) -> i32;
    fn AudioUnitSetProperty(
        unit: AudioComponentInstance,
        property_id: u32,
        scope: u32,
        element: u32,
        value: *const c_void,
        value_size: u32,
    ) -> i32;
    fn AudioUnitInitialize(unit: AudioComponentInstance) -> i32;
    fn AudioOutputUnitStart(unit: AudioComponentInstance) -> i32;
    fn AudioOutputUnitStop(unit: AudioComponentInstance) -> i32;
    fn AudioUnitUninitialize(unit: AudioComponentInstance) -> i32;
    fn AudioComponentInstanceDispose(unit: AudioComponentInstance) -> i32;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qual_data_size: u32,
        qual_data: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

// ---- Render callback -----------------------------------------------------

struct CallbackContext {
    ring: std::sync::Arc<PcmRing>,
    mute: std::sync::Arc<MuteState>,
    generation: std::sync::Arc<AtomicU64>,
    callbacks: AtomicU64,
    silence_frames: AtomicU64,
    last_callback_ms: AtomicU64,
}

extern "C" fn render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    _in_time_stamp: *const c_void,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    let ctx = unsafe { &*(in_ref_con as *const CallbackContext) };
    let frames = in_number_frames as usize;
    let samples = frames * CHANNELS;
    let now = now_ms();
    ctx.last_callback_ms.store(now, Ordering::Relaxed);
    ctx.callbacks.fetch_add(1, Ordering::Relaxed);

    if io_data.is_null() {
        return 0;
    }
    let buffer = unsafe { &mut (*io_data).buffers[0] };
    let out = if buffer.data.is_null() {
        return 0;
    } else {
        unsafe { std::slice::from_raw_parts_mut(buffer.data as *mut i16, samples) }
    };

    let copied = ctx.ring.read(out);
    if ctx.mute.get() {
        // Discard whatever was copied so unmute never replays stale PCM.
        for sample in out.iter_mut() {
            *sample = 0;
        }
        ctx.silence_frames
            .fetch_add((samples / CHANNELS) as u64, Ordering::Relaxed);
    } else if copied < samples {
        ctx.silence_frames
            .fetch_add(((samples - copied) / CHANNELS) as u64, Ordering::Relaxed);
    }
    buffer.data_byte_size = (samples * std::mem::size_of::<i16>()) as u32;
    0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- Backend -------------------------------------------------------------

pub struct AudioUnitOutput {
    unit: AudioComponentInstance,
    context: Box<CallbackContext>,
    generation: Arc<AtomicU64>,
    restarts: u64,
    last_reason: Option<AudioRestartReason>,
    output_device: u32,
}

impl AudioUnitOutput {
    pub fn new(ring: Arc<PcmRing>, mute: Arc<MuteState>) -> Result<Self> {
        let generation = Arc::new(AtomicU64::new(1));
        let context = Box::new(CallbackContext {
            ring,
            mute,
            generation: generation.clone(),
            callbacks: AtomicU64::new(0),
            silence_frames: AtomicU64::new(0),
            last_callback_ms: AtomicU64::new(0),
        });
        Ok(Self {
            unit: ptr::null_mut(),
            context,
            generation,
            restarts: 0,
            last_reason: None,
            output_device: 0,
        })
    }

    fn pcm_format() -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: kAudioFormatLinearPCM,
            // kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked
            format_flags: (1 << 2) | (1 << 3),
            bytes_per_packet: (CHANNELS * 2) as u32,
            frames_per_packet: 1,
            bytes_per_frame: (CHANNELS * 2) as u32,
            channels_per_frame: CHANNELS as u32,
            bits_per_channel: 16,
            reserved: 0,
        }
    }

    fn default_output_device() -> Option<u32> {
        let address = AudioObjectPropertyAddress {
            selector: kAudioHardwarePropertyDefaultOutputDevice,
            scope: kAudioObjectPropertyScopeGlobal,
            element: kAudioObjectPropertyElementMain,
        };
        let mut device: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut device as *mut u32 as *mut c_void,
            )
        };
        if status == 0 {
            Some(device)
        } else {
            None
        }
    }

    fn create_unit(&mut self) -> Result<()> {
        if !self.unit.is_null() {
            return Ok(());
        }
        let desc = AudioComponentDescription {
            component_type: kAudioUnitType_Output,
            component_sub_type: kAudioUnitSubType_DefaultOutput,
            component_manufacturer: kAudioUnitManufacturer_Apple,
            component_flags: 0,
            component_flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
        if component.is_null() {
            bail!("default output AudioUnit component not found");
        }
        let mut unit: AudioComponentInstance = ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
        if status != 0 || unit.is_null() {
            bail!("AudioComponentInstanceNew failed with OSStatus {status}");
        }
        self.unit = unit;

        let format = Self::pcm_format();
        let status = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                &format as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if status != 0 {
            unsafe { AudioComponentInstanceDispose(unit) };
            self.unit = ptr::null_mut();
            bail!("setting AudioUnit stream format failed with OSStatus {status}");
        }

        let callback = AURenderCallbackStruct {
            input_proc: Some(render_callback),
            input_proc_ref_con: (&mut *self.context as *mut CallbackContext).cast(),
        };
        let status = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &callback as *const _ as *const c_void,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            )
        };
        if status != 0 {
            unsafe { AudioComponentInstanceDispose(unit) };
            self.unit = ptr::null_mut();
            bail!("setting AudioUnit render callback failed with OSStatus {status}");
        }

        let status = unsafe { AudioUnitInitialize(unit) };
        if status != 0 {
            unsafe { AudioComponentInstanceDispose(unit) };
            self.unit = ptr::null_mut();
            bail!("AudioUnitInitialize failed with OSStatus {status}");
        }
        self.output_device = Self::default_output_device().unwrap_or(0);
        Ok(())
    }

    fn destroy_unit(&mut self) {
        if self.unit.is_null() {
            return;
        }
        let unit = self.unit;
        self.unit = ptr::null_mut();
        unsafe {
            let _ = AudioOutputUnitStop(unit);
            let _ = AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }
}

// The AudioUnit instance is used only from the task that owns this struct;
// the render callback runs on CoreAudio's thread with its own context.
unsafe impl Send for AudioUnitOutput {}

impl AudioOutput for AudioUnitOutput {
    fn backend_name(&self) -> &'static str {
        "audiounit"
    }

    fn prepare(&mut self) -> Result<()> {
        self.create_unit()
    }

    fn start(&mut self) -> Result<()> {
        if self.unit.is_null() {
            self.create_unit()?;
        }
        let status = unsafe { AudioOutputUnitStart(self.unit) };
        if status != 0 {
            bail!("AudioOutputUnitStart failed with OSStatus {status}");
        }
        Ok(())
    }

    fn set_muted(&self, muted: bool) {
        self.context.mute.set(muted);
    }

    fn is_muted(&self) -> bool {
        self.context.mute.get()
    }

    fn stop(&mut self) {
        self.destroy_unit();
    }

    fn frame_stats(&self) -> AudioOutputStats {
        let ring = self.context.ring.stats();
        AudioOutputStats {
            backend: self.backend_name(),
            callbacks: self.context.callbacks.load(Ordering::Relaxed),
            silence_frames: self.context.silence_frames.load(Ordering::Relaxed),
            underruns: ring.underruns,
            overruns: ring.overruns,
            frames_available: ring.frames_available,
            capacity_frames: ring.capacity_frames,
            restarts: self.restarts,
            last_restart_reason: self.last_reason,
            output_device: self.output_device,
        }
    }

    fn ring(&self) -> &PcmRing {
        &self.context.ring
    }

    fn restart(&mut self, reason: AudioRestartReason) -> Result<()> {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.context
            .generation
            .store(self.generation.load(Ordering::Relaxed), Ordering::Release);
        self.destroy_unit();
        self.create_unit()?;
        let status = unsafe { AudioOutputUnitStart(self.unit) };
        if status != 0 {
            bail!("AudioOutputUnitStart after restart failed with OSStatus {status}");
        }
        self.restarts += 1;
        self.last_reason = Some(reason);
        Ok(())
    }

    fn current_output_device(&self) -> Option<u32> {
        Self::default_output_device()
    }
}

impl Drop for AudioUnitOutput {
    fn drop(&mut self) {
        self.destroy_unit();
    }
}
