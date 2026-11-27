// language: Rust, file: src/audio/mod.rs, target: macOS AudioToolbox
pub mod audiounit;
pub mod audioqueue;
pub mod jitter;
pub mod output;
pub mod reorder;
pub mod ring_buffer;

use anyhow::{bail, Result};
use std::{
    collections::VecDeque,
    ffi::c_void,
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
const FRAMES_PER_PACKET: u32 = 480;
const PCM_BYTES: usize = FRAMES_PER_PACKET as usize * CHANNELS as usize * 2;

fn esds_cookie_for_rate(sample_rate: f64) -> [u8; 41] {
    let asc: [u8; 4] = if (sample_rate - 44_100.0).abs() < 1.0 {
        [0xF8, 0xE8, 0x50, 0x00]
    } else {
        [0xF8, 0xE6, 0x50, 0x00]
    };
    [
        0x03, 0x80, 0x80, 0x80, 0x24, 0x00, 0x00, 0x00, 0x04, 0x80, 0x80, 0x80, 0x16, 0x40, 0x14,
        0x00, 0x18, 0x00, 0x00, 0x01, 0xf4, 0x00, 0x00, 0x01, 0xf4, 0x00, 0x05, 0x80, 0x80, 0x80,
        0x04, asc[0], asc[1], asc[2], asc[3], 0x06, 0x80, 0x80, 0x80, 0x01, 0x02,
    ]
}

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
    source_sample_rate: f64,
    resample_phase: f64,
    prev_stereo_frame: Option<[i16; 2]>,
}
unsafe impl Send for AacEldDecoder {}

impl AacEldDecoder {
    pub fn new() -> Result<Self> {
        Self::new_with_source_rate(SAMPLE_RATE)
    }

    /// Creates an AAC-ELD decoder from `source_sample_rate` (e.g. `44_100.0` for
    /// AirPlay or `48_000.0` for CoreDevice) returning 48 kHz interleaved stereo `i16` PCM.
    /// AudioToolbox converts 1:1 at `source_sample_rate` so it never triggers early EOF.
    pub fn new_with_source_rate(source_sample_rate: f64) -> Result<Self> {
        let source = AudioStreamBasicDescription {
            sample_rate: source_sample_rate,
            format_id: fourcc(b"aace"),
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: FRAMES_PER_PACKET,
            bytes_per_frame: 0,
            channels_per_frame: CHANNELS,
            bits_per_channel: 0,
            reserved: 0,
        };
        let destination = pcm_format_with_rate(source_sample_rate);
        let mut converter = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&source, &destination, &mut converter) };
        if status != 0 || converter.is_null() {
            bail!("AudioConverterNew AAC-ELD ({source_sample_rate}Hz) failed with OSStatus {status}");
        }
        let cookie = esds_cookie_for_rate(source_sample_rate);
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                fourcc(b"dmgc"),
                cookie.len() as u32,
                cookie.as_ptr().cast(),
            )
        };
        if status != 0 {
            tracing::debug!(status, "AudioConverter rejected AAC-ELD magic cookie");
        }
        Ok(Self {
            converter,
            source_sample_rate,
            resample_phase: 0.0,
            prev_stereo_frame: None,
        })
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
        if (self.source_sample_rate - SAMPLE_RATE).abs() < 1.0 || pcm.len() < 4 {
            return Ok(pcm);
        }
        Ok(resample_stereo_pcm(
            &pcm,
            self.source_sample_rate,
            SAMPLE_RATE,
            &mut self.resample_phase,
            &mut self.prev_stereo_frame,
        ))
    }
}

fn resample_stereo_pcm(
    input_bytes: &[u8],
    src_rate: f64,
    dst_rate: f64,
    phase: &mut f64,
    prev_frame: &mut Option<[i16; 2]>,
) -> Vec<u8> {
    let frames: Vec<[i16; 2]> = input_bytes
        .chunks_exact(4)
        .map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])])
        .collect();
    if frames.is_empty() {
        return Vec::new();
    }

    let step = src_rate / dst_rate;
    let est_out = ((frames.len() as f64 / step) as usize) + 4;
    let mut out = Vec::with_capacity(est_out * 4);
    let last_idx = (frames.len() - 1) as f64;

    while *phase < last_idx {
        let (s0, s1, t) = if *phase < 0.0 {
            (prev_frame.unwrap_or(frames[0]), frames[0], *phase + 1.0)
        } else {
            let idx = phase.floor() as usize;
            (frames[idx], frames[idx + 1], *phase - idx as f64)
        };
        let l = (s0[0] as f64 + t * (s1[0] as f64 - s0[0] as f64)).round() as i16;
        let r = (s0[1] as f64 + t * (s1[1] as f64 - s0[1] as f64)).round() as i16;
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
        *phase += step;
    }

    *prev_frame = frames.last().copied();
    *phase -= frames.len() as f64;
    out
}

impl Drop for AacEldDecoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

struct QueueContext {
    free: Mutex<VecDeque<usize>>,
    /// Incremented every time AudioToolbox returns a consumed buffer. A stalled
    /// counter means the output queue stopped even while we keep enqueuing.
    buffers_returned: Arc<AtomicU64>,
}

extern "C" fn queue_output(
    user_data: *mut c_void,
    _queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    let context = unsafe { &*(user_data as *const QueueContext) };
    context.buffers_returned.fetch_add(1, Ordering::Relaxed);
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
    pub fn new(buffers_returned: Arc<AtomicU64>) -> Result<Self> {
        let mut context = Box::new(QueueContext {
            free: Mutex::new(VecDeque::new()),
            buffers_returned,
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
        // AudioQueue can transition to an underrun-stopped state when the
        // device is temporarily silent or the machine is under load. Starting
        // an already-running queue is harmless and guarantees recovery once a
        // new PCM buffer has been enqueued.
        let status = unsafe { AudioQueueStart(self.queue, ptr::null()) };
        if status != 0 {
            bail!("AudioQueueStart after enqueue failed with OSStatus {status}");
        }
        Ok(())
    }

    /// Keep AudioQueue running while muted by enqueueing one 10 ms silent PCM
    /// frame. Software silence avoids output-unit volume/state transitions that
    /// do not reliably recover on every macOS/Hackintosh audio driver.
    pub fn play_silence(&mut self) -> Result<()> {
        const SILENCE: [u8; PCM_BYTES] = [0; PCM_BYTES];
        self.play(&SILENCE)
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
    pcm_format_with_rate(SAMPLE_RATE)
}

fn pcm_format_with_rate(sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate,
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{
        AacEldDecoder, AudioBuffer, AudioBufferList, AudioConverterDispose,
        AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef, AudioPlayer,
        AudioStreamBasicDescription, AudioStreamPacketDescription, CHANNELS, FRAMES_PER_PACKET,
        PCM_BYTES, fourcc, pcm_format_with_rate,
    };
    use std::{
        ffi::c_void,
        ptr,
        sync::{Arc, atomic::AtomicU64},
    };

    extern "C" fn encode_pcm_input(
        _converter: AudioConverterRef,
        packet_count: *mut u32,
        data: *mut AudioBufferList,
        _description: *mut *mut AudioStreamPacketDescription,
        user_data: *mut c_void,
    ) -> i32 {
        let buf = unsafe { &*(user_data as *const Vec<u8>) };
        unsafe {
            *packet_count = FRAMES_PER_PACKET;
            (*data).number_buffers = 1;
            (*data).buffers[0] = AudioBuffer {
                number_channels: CHANNELS,
                data_byte_size: buf.len() as u32,
                data: buf.as_ptr().cast_mut().cast(),
            };
        }
        0
    }

    #[test]
    fn aac_eld_44100_decodes_multiple_consecutive_packets_without_eof() {
        let src = pcm_format_with_rate(44_100.0);
        let dst = AudioStreamBasicDescription {
            sample_rate: 44_100.0,
            format_id: fourcc(b"aace"),
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: FRAMES_PER_PACKET,
            bytes_per_frame: 0,
            channels_per_frame: CHANNELS,
            bits_per_channel: 0,
            reserved: 0,
        };
        let mut encoder = ptr::null_mut();
        assert_eq!(unsafe { AudioConverterNew(&src, &dst, &mut encoder) }, 0);

        let pcm_in = vec![0_u8; PCM_BYTES];
        let mut encoded = vec![0_u8; 512];
        let mut out_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: CHANNELS,
                data_byte_size: encoded.len() as u32,
                data: encoded.as_mut_ptr().cast(),
            }],
        };
        let mut out_pkts = 1_u32;
        let mut out_desc = AudioStreamPacketDescription {
            start_offset: 0,
            variable_frames_in_packet: 0,
            data_byte_size: 0,
        };
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                encoder,
                encode_pcm_input,
                (&pcm_in as *const Vec<u8>).cast_mut().cast(),
                &mut out_pkts,
                &mut out_list,
                &mut out_desc,
            )
        };
        assert_eq!(status, 0);
        let au_len = out_list.buffers[0].data_byte_size as usize;
        assert!(au_len > 0);
        let au = encoded[..au_len].to_vec();
        unsafe { AudioConverterDispose(encoder) };

        let mut decoder = AacEldDecoder::new_with_source_rate(44_100.0).expect("44.1k decoder");
        for i in 0..5 {
            let decoded = decoder.decode(&au).expect("decode 44.1kHz AAC-ELD");
            assert!(
                decoded.len() >= 2080,
                "packet {i} produced only {} bytes (EOF regression)",
                decoded.len()
            );
        }
    }

    #[test]
    fn audio_queue_can_play_silence_then_resume_pcm() {
        let mut player =
            AudioPlayer::new(Arc::new(AtomicU64::new(0))).expect("create AudioQueue");
        let silence = vec![0_u8; PCM_BYTES];
        player.play(&silence).expect("initial enqueue");
        player.play_silence().expect("software-muted enqueue");
        player.play(&silence).expect("enqueue after unmute");
    }
}
