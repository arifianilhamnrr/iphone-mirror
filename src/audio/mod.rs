// language: Rust, file: src/audio/mod.rs, target: macOS AudioToolbox
use anyhow::{bail, Result};
use std::{collections::VecDeque, ffi::c_void, ptr, sync::Mutex};

const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
const FRAMES_PER_PACKET: u32 = 480;
const PCM_BYTES: usize = FRAMES_PER_PACKET as usize * CHANNELS as usize * 2;
const AAC_ELD_COOKIE: [u8; 4] = [0xF8, 0xE6, 0x40, 0x00];

const fn fourcc(value: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*value)
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

#[repr(C)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

type AudioConverterRef = *mut c_void;
type AudioQueueRef = *mut c_void;

#[repr(C)]
struct AudioQueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut AudioStreamPacketDescription,
    packet_description_count: u32,
}
type AudioQueueBufferRef = *mut AudioQueueBuffer;

type ConverterInputCallback = extern "C" fn(
    AudioConverterRef,
    *mut u32,
    *mut AudioBufferList,
    *mut *mut AudioStreamPacketDescription,
    *mut c_void,
) -> i32;
type QueueOutputCallback = extern "C" fn(*mut c_void, AudioQueueRef, AudioQueueBufferRef);

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioConverterNew(
        source: *const AudioStreamBasicDescription,
        destination: *const AudioStreamBasicDescription,
        converter: *mut AudioConverterRef,
    ) -> i32;
    fn AudioConverterSetProperty(
        converter: AudioConverterRef,
        property: u32,
        size: u32,
        data: *const c_void,
    ) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: AudioConverterRef,
        input: ConverterInputCallback,
        user_data: *mut c_void,
        output_packets: *mut u32,
        output: *mut AudioBufferList,
        packet_description: *mut AudioStreamPacketDescription,
    ) -> i32;
    fn AudioConverterDispose(converter: AudioConverterRef) -> i32;

    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: QueueOutputCallback,
        user_data: *mut c_void,
        callback_run_loop: *mut c_void,
        callback_mode: *mut c_void,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(
        queue: AudioQueueRef,
        capacity: u32,
        buffer: *mut AudioQueueBufferRef,
    ) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: AudioQueueBufferRef,
        descriptions: u32,
        packet_descriptions: *const AudioStreamPacketDescription,
    ) -> i32;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> i32;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: bool) -> i32;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: bool) -> i32;
}

struct InputContext {
    data: *const u8,
    len: usize,
    consumed: bool,
    description: AudioStreamPacketDescription,
}

extern "C" fn converter_input(
    _converter: AudioConverterRef,
    packet_count: *mut u32,
    data: *mut AudioBufferList,
    description: *mut *mut AudioStreamPacketDescription,
    user_data: *mut c_void,
) -> i32 {
    let context = unsafe { &mut *(user_data as *mut InputContext) };
    if context.consumed {
        unsafe { *packet_count = 0 };
        return 0;
    }
    context.consumed = true;
    context.description.data_byte_size = context.len as u32;
    unsafe {
        *packet_count = 1;
        (*data).number_buffers = 1;
        (*data).buffers[0] = AudioBuffer {
            number_channels: CHANNELS,
            data_byte_size: context.len as u32,
            data: context.data.cast_mut().cast(),
        };
        if !description.is_null() {
            *description = &mut context.description;
        }
    }
    0
}

pub struct AacEldDecoder {
    converter: AudioConverterRef,
}
unsafe impl Send for AacEldDecoder {}

impl AacEldDecoder {
    pub fn new() -> Result<Self> {
        let source = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: fourcc(b"aace"),
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: FRAMES_PER_PACKET,
            bytes_per_frame: 0,
            channels_per_frame: CHANNELS,
            bits_per_channel: 0,
            reserved: 0,
        };
        let destination = pcm_format();
        let mut converter = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&source, &destination, &mut converter) };
        if status != 0 || converter.is_null() {
            bail!("AudioConverterNew AAC-ELD failed with OSStatus {status}");
        }
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                fourcc(b"dmgc"),
                AAC_ELD_COOKIE.len() as u32,
                AAC_ELD_COOKIE.as_ptr().cast(),
            )
        };
        if status != 0 {
            // The reference implementation ignores this too: the cookie is
            // already advertised out-of-band and some AudioToolbox builds
            // reject re-setting it while still decoding AAC-ELD correctly.
            tracing::debug!(status, "AudioConverter rejected AAC-ELD magic cookie");
        }
        Ok(Self { converter })
    }

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<u8>> {
        let mut pcm = vec![0_u8; PCM_BYTES];
        let mut context = InputContext {
            data: access_unit.as_ptr(),
            len: access_unit.len(),
            consumed: false,
            description: AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: access_unit.len() as u32,
            },
        };
        let mut output = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: CHANNELS,
                data_byte_size: pcm.len() as u32,
                data: pcm.as_mut_ptr().cast(),
            }],
        };
        let mut packets = FRAMES_PER_PACKET;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                converter_input,
                (&mut context as *mut InputContext).cast(),
                &mut packets,
                &mut output,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            bail!("AAC-ELD decode failed with OSStatus {status}");
        }
        pcm.truncate(output.buffers[0].data_byte_size as usize);
        Ok(pcm)
    }
}

impl Drop for AacEldDecoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

struct QueueContext {
    free: Mutex<VecDeque<usize>>,
}

extern "C" fn queue_output(
    user_data: *mut c_void,
    _queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    let context = unsafe { &*(user_data as *const QueueContext) };
    if let Ok(mut free) = context.free.lock() {
        free.push_back(buffer as usize);
    }
}

pub struct AudioPlayer {
    queue: AudioQueueRef,
    context: Box<QueueContext>,
    dropped: u64,
}
unsafe impl Send for AudioPlayer {}

impl AudioPlayer {
    pub fn new() -> Result<Self> {
        let mut context = Box::new(QueueContext {
            free: Mutex::new(VecDeque::new()),
        });
        let mut queue = ptr::null_mut();
        let status = unsafe {
            AudioQueueNewOutput(
                &pcm_format(),
                queue_output,
                (&mut *context as *mut QueueContext).cast(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut queue,
            )
        };
        if status != 0 || queue.is_null() {
            bail!("AudioQueueNewOutput failed with OSStatus {status}");
        }
        for _ in 0..12 {
            let mut buffer = ptr::null_mut();
            let status = unsafe { AudioQueueAllocateBuffer(queue, PCM_BYTES as u32, &mut buffer) };
            if status != 0 {
                unsafe { AudioQueueDispose(queue, true) };
                bail!("AudioQueueAllocateBuffer failed with OSStatus {status}");
            }
            context
                .free
                .lock()
                .expect("audio queue mutex")
                .push_back(buffer as usize);
        }
        let status = unsafe { AudioQueueStart(queue, ptr::null()) };
        if status != 0 {
            unsafe { AudioQueueDispose(queue, true) };
            bail!("AudioQueueStart failed with OSStatus {status}");
        }
        Ok(Self {
            queue,
            context,
            dropped: 0,
        })
    }

    pub fn play(&mut self, pcm: &[u8]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let Some(raw) = self
            .context
            .free
            .lock()
            .expect("audio queue mutex")
            .pop_front()
        else {
            self.dropped += 1;
            return Ok(());
        };
        let buffer = raw as AudioQueueBufferRef;
        let size = pcm.len().min(PCM_BYTES);
        unsafe {
            ptr::copy_nonoverlapping(pcm.as_ptr(), (*buffer).audio_data.cast(), size);
            (*buffer).audio_data_byte_size = size as u32;
        }
        let status = unsafe { AudioQueueEnqueueBuffer(self.queue, buffer, 0, ptr::null()) };
        if status != 0 {
            self.context
                .free
                .lock()
                .expect("audio queue mutex")
                .push_back(raw);
            bail!("AudioQueueEnqueueBuffer failed with OSStatus {status}");
        }
        Ok(())
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        unsafe {
            AudioQueueStop(self.queue, true);
            AudioQueueDispose(self.queue, true);
        }
    }
}

fn pcm_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate: SAMPLE_RATE,
        format_id: fourcc(b"lpcm"),
        format_flags: (1 << 2) | (1 << 3),
        bytes_per_packet: CHANNELS * 2,
        frames_per_packet: 1,
        bytes_per_frame: CHANNELS * 2,
        channels_per_frame: CHANNELS,
        bits_per_channel: 16,
        reserved: 0,
    }
}
