// language: Rust, file: src/audio/mod.rs, target: macOS AudioToolbox & Linux PipeWire/Pulse/ALSA
#[cfg(target_os = "macos")]
pub mod audiounit;
pub mod audioqueue;
pub mod jitter;
pub mod output;
pub mod reorder;
pub mod ring_buffer;

use anyhow::{bail, Result};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

#[cfg(target_os = "macos")]
use std::{collections::VecDeque, sync::Mutex};

#[cfg(not(target_os = "macos"))]
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::Mutex,
};

const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
const FRAMES_PER_PACKET: u32 = 480;
const PCM_BYTES: usize = FRAMES_PER_PACKET as usize * CHANNELS as usize * 2;

#[allow(dead_code)]
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

#[cfg(target_os = "macos")]
const fn fourcc(value: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*value)
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

#[cfg(target_os = "macos")]
type AudioConverterRef = *mut c_void;
#[cfg(target_os = "macos")]
type AudioQueueRef = *mut c_void;

#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
type AudioQueueBufferRef = *mut AudioQueueBuffer;

#[cfg(target_os = "macos")]
type ConverterInputCallback = extern "C" fn(
    AudioConverterRef,
    *mut u32,
    *mut AudioBufferList,
    *mut *mut AudioStreamPacketDescription,
    *mut c_void,
) -> i32;
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
struct InputContext {
    data: *const u8,
    len: usize,
    consumed: bool,
    description: AudioStreamPacketDescription,
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
pub struct AacEldDecoder {
    converter: AudioConverterRef,
    source_sample_rate: f64,
    resample_phase: f64,
    prev_stereo_frame: Option<[i16; 2]>,
}

#[cfg(not(target_os = "macos"))]
pub struct AacEldDecoder {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pcm_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    seq: u32,
    source_sample_rate: f64,
}

unsafe impl Send for AacEldDecoder {}

impl AacEldDecoder {
    pub fn new() -> Result<Self> {
        Self::new_with_source_rate(SAMPLE_RATE)
    }

    /// Creates an AAC-ELD decoder from `source_sample_rate` (e.g. `44_100.0` for
    /// AirPlay or `48_000.0` for CoreDevice) returning 48 kHz interleaved stereo `i16` PCM.
    pub fn new_with_source_rate(source_sample_rate: f64) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
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

        #[cfg(not(target_os = "macos"))]
        {
            let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
            let spawned = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "quiet",
                    "-fflags",
                    "nobuffer",
                    "-flags",
                    "low_delay",
                    "-f",
                    "mp4",
                    "-i",
                    "pipe:0",
                    "-f",
                    "s16le",
                    "-ar",
                    "48000",
                    "-ac",
                    "2",
                    "pipe:1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            match spawned {
                Ok(mut child) => {
                    let mut stdin = child.stdin.take();
                    let stdout = child.stdout.take();
                    if let Some(ref mut sin) = stdin {
                        let init = build_fmp4_init(source_sample_rate);
                        let _ = sin.write_all(&init);
                        let _ = sin.flush();
                    }
                    if let Some(mut sout) = stdout {
                        let _ = std::thread::Builder::new()
                            .name("linux-aac-eld-reader".to_string())
                            .spawn(move || {
                                let mut buf = vec![0_u8; PCM_BYTES];
                                while sout.read_exact(&mut buf).is_ok() {
                                    if pcm_tx.try_send(buf.clone()).is_err() {
                                        break;
                                    }
                                }
                            });
                    }
                    Ok(Self {
                        child: Some(child),
                        stdin,
                        pcm_rx,
                        seq: 1,
                        source_sample_rate,
                    })
                }
                Err(error) => {
                    tracing::warn!(%error, "ffmpeg unavailable for Linux AAC-ELD decoding; audio disabled");
                    Ok(Self {
                        child: None,
                        stdin: None,
                        pcm_rx,
                        seq: 1,
                        source_sample_rate,
                    })
                }
            }
        }
    }

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<u8>> {
        #[cfg(target_os = "macos")]
        {
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

        #[cfg(not(target_os = "macos"))]
        {
            if let Some(stdin) = self.stdin.as_mut() {
                let frag = build_fmp4_fragment(self.seq, self.source_sample_rate, access_unit);
                self.seq = self.seq.wrapping_add(1);
                if stdin.write_all(&frag).is_err() || stdin.flush().is_err() {
                    self.stdin = None;
                }
            }
            if let Ok(pcm) = self.pcm_rx.try_recv() {
                return Ok(pcm);
            }
            // Wait briefly for the first packet so live stream doesn't slip behind
            match self
                .pcm_rx
                .recv_timeout(std::time::Duration::from_millis(8))
            {
                Ok(pcm) => Ok(pcm),
                Err(_) => Ok(vec![0_u8; PCM_BYTES]),
            }
        }
    }
}

#[allow(dead_code)]
fn resample_stereo_pcm(
    input_bytes: &[u8],
    src_rate: f64,
    dst_rate: f64,
    phase: &mut f64,
    prev_frame: &mut Option<[i16; 2]>,
) -> Vec<u8> {
    let frames: Vec<[i16; 2]> = input_bytes
        .chunks_exact(4)
        .map(|c| {
            [
                i16::from_le_bytes([c[0], c[1]]),
                i16::from_le_bytes([c[2], c[3]]),
            ]
        })
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
        #[cfg(target_os = "macos")]
        unsafe {
            AudioConverterDispose(self.converter);
        }
        #[cfg(not(target_os = "macos"))]
        {
            drop(self.stdin.take());
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn mp4_box(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(payload);
    out
}

#[cfg(not(target_os = "macos"))]
fn mp4_fullbox(tag: &[u8; 4], version: u8, flags: [u8; 3], payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.push(version);
    inner.extend_from_slice(&flags);
    inner.extend_from_slice(payload);
    mp4_box(tag, &inner)
}

#[cfg(not(target_os = "macos"))]
fn build_fmp4_init(sample_rate: f64) -> Vec<u8> {
    let sr = sample_rate.round() as u32;
    let ftyp = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso5");

    let mut mvhd_payload = vec![0_u8; 92];
    mvhd_payload[8..12].copy_from_slice(&sr.to_be_bytes());
    mvhd_payload[24..28].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[28..30].copy_from_slice(&0x0100_u16.to_be_bytes());
    mvhd_payload[40..44].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[56..60].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    mvhd_payload[72..76].copy_from_slice(&0x4000_0000_u32.to_be_bytes());
    mvhd_payload[88..92].copy_from_slice(&2_u32.to_be_bytes());
    let mvhd = mp4_fullbox(b"mvhd", 0, [0; 3], &mvhd_payload);

    let mut tkhd_payload = vec![0_u8; 80];
    tkhd_payload[8..12].copy_from_slice(&1_u32.to_be_bytes());
    tkhd_payload[36..40].copy_from_slice(&0x0100_0000_u32.to_be_bytes());
    tkhd_payload[44..48].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    tkhd_payload[60..64].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    tkhd_payload[76..80].copy_from_slice(&0x4000_0000_u32.to_be_bytes());
    let tkhd = mp4_fullbox(b"tkhd", 0, [0, 0, 7], &tkhd_payload);

    let mut mdhd_payload = vec![0_u8; 20];
    mdhd_payload[8..12].copy_from_slice(&sr.to_be_bytes());
    mdhd_payload[16..20].copy_from_slice(&0x55C4_0000_u32.to_be_bytes());
    let mdhd = mp4_fullbox(b"mdhd", 0, [0; 3], &mdhd_payload);

    let mut hdlr_payload = vec![0_u8; 25];
    hdlr_payload[4..8].copy_from_slice(b"soun");
    hdlr_payload[24] = 0;
    let hdlr = mp4_fullbox(b"hdlr", 0, [0; 3], &hdlr_payload);

    let smhd = mp4_fullbox(b"smhd", 0, [0; 3], &[0; 4]);
    let url = mp4_fullbox(b"url ", 0, [0, 0, 1], &[]);
    let mut dref_p = vec![0, 0, 0, 1];
    dref_p.extend_from_slice(&url);
    let dref = mp4_fullbox(b"dref", 0, [0; 3], &dref_p);
    let dinf = mp4_box(b"dinf", &dref);

    let esds = mp4_fullbox(b"esds", 0, [0; 3], &esds_cookie_for_rate(sample_rate));
    let mut mp4a_payload = vec![0_u8; 28];
    mp4a_payload[6..8].copy_from_slice(&1_u16.to_be_bytes());
    mp4a_payload[16..18].copy_from_slice(&2_u16.to_be_bytes());
    mp4a_payload[18..20].copy_from_slice(&16_u16.to_be_bytes());
    mp4a_payload[24..26].copy_from_slice(&(sr as u16).to_be_bytes());
    mp4a_payload.extend_from_slice(&esds);
    let mp4a = mp4_box(b"mp4a", &mp4a_payload);

    let mut stsd_p = vec![0, 0, 0, 1];
    stsd_p.extend_from_slice(&mp4a);
    let stsd = mp4_fullbox(b"stsd", 0, [0; 3], &stsd_p);
    let stts = mp4_fullbox(b"stts", 0, [0; 3], &[0; 4]);
    let stsc = mp4_fullbox(b"stsc", 0, [0; 3], &[0; 4]);
    let stsz = mp4_fullbox(b"stsz", 0, [0; 3], &[0; 8]);
    let stco = mp4_fullbox(b"stco", 0, [0; 3], &[0; 4]);
    let mut stbl_p = Vec::new();
    stbl_p.extend_from_slice(&stsd);
    stbl_p.extend_from_slice(&stts);
    stbl_p.extend_from_slice(&stsc);
    stbl_p.extend_from_slice(&stsz);
    stbl_p.extend_from_slice(&stco);
    let stbl = mp4_box(b"stbl", &stbl_p);

    let mut minf_p = Vec::new();
    minf_p.extend_from_slice(&smhd);
    minf_p.extend_from_slice(&dinf);
    minf_p.extend_from_slice(&stbl);
    let minf = mp4_box(b"minf", &minf_p);

    let mut mdia_p = Vec::new();
    mdia_p.extend_from_slice(&mdhd);
    mdia_p.extend_from_slice(&hdlr);
    mdia_p.extend_from_slice(&minf);
    let mdia = mp4_box(b"mdia", &mdia_p);

    let mut trak_p = Vec::new();
    trak_p.extend_from_slice(&tkhd);
    trak_p.extend_from_slice(&mdia);
    let trak = mp4_box(b"trak", &trak_p);

    let mut trex_p = vec![0_u8; 20];
    trex_p[0..4].copy_from_slice(&1_u32.to_be_bytes());
    trex_p[4..8].copy_from_slice(&1_u32.to_be_bytes());
    trex_p[8..12].copy_from_slice(&FRAMES_PER_PACKET.to_be_bytes());
    let trex = mp4_fullbox(b"trex", 0, [0; 3], &trex_p);
    let mvex = mp4_box(b"mvex", &trex);

    let mut moov_p = Vec::new();
    moov_p.extend_from_slice(&mvhd);
    moov_p.extend_from_slice(&trak);
    moov_p.extend_from_slice(&mvex);
    let moov = mp4_box(b"moov", &moov_p);

    let mut out = ftyp;
    out.extend_from_slice(&moov);
    out
}

#[cfg(not(target_os = "macos"))]
fn build_fmp4_fragment(seq: u32, _sample_rate: f64, au: &[u8]) -> Vec<u8> {
    let mfhd = mp4_fullbox(b"mfhd", 0, [0; 3], &seq.to_be_bytes());
    let tfhd = mp4_fullbox(b"tfhd", 0, [0x02, 0x00, 0x00], &1_u32.to_be_bytes());
    let tfdt = mp4_fullbox(
        b"tfdt",
        1,
        [0; 3],
        &((seq.saturating_sub(1) as u64) * FRAMES_PER_PACKET as u64).to_be_bytes(),
    );

    // trun: sample_count=1, data_offset placeholder, duration, size
    let mut trun_p = Vec::with_capacity(16);
    trun_p.extend_from_slice(&1_u32.to_be_bytes());
    trun_p.extend_from_slice(&0_i32.to_be_bytes()); // data_offset patched below
    trun_p.extend_from_slice(&FRAMES_PER_PACKET.to_be_bytes());
    trun_p.extend_from_slice(&(au.len() as u32).to_be_bytes());
    let trun = mp4_fullbox(b"trun", 0, [0x00, 0x03, 0x01], &trun_p);

    let mut traf_p = Vec::new();
    traf_p.extend_from_slice(&tfhd);
    traf_p.extend_from_slice(&tfdt);
    traf_p.extend_from_slice(&trun);
    let traf = mp4_box(b"traf", &traf_p);

    let mut moof_p = Vec::new();
    moof_p.extend_from_slice(&mfhd);
    moof_p.extend_from_slice(&traf);
    let mut moof = mp4_box(b"moof", &moof_p);

    // data_offset is moof.len() + 8 (mdat header)
    let data_offset = (moof.len() + 8) as i32;
    let offset_pos = moof.len() - 12;
    moof[offset_pos..offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mdat = mp4_box(b"mdat", au);
    moof.extend_from_slice(&mdat);
    moof
}

#[cfg(target_os = "macos")]
struct QueueContext {
    free: Mutex<VecDeque<usize>>,
    /// Incremented every time AudioToolbox returns a consumed buffer. A stalled
    /// counter means the output queue stopped even while we keep enqueuing.
    buffers_returned: Arc<AtomicU64>,
}

#[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
    queue: AudioQueueRef,
    #[cfg(target_os = "macos")]
    context: Box<QueueContext>,
    #[cfg(not(target_os = "macos"))]
    sink_child: Option<Child>,
    #[cfg(not(target_os = "macos"))]
    sink_stdin: Option<Mutex<ChildStdin>>,
    #[cfg(not(target_os = "macos"))]
    buffers_returned: Arc<AtomicU64>,
    dropped: u64,
}
unsafe impl Send for AudioPlayer {}

impl AudioPlayer {
    pub fn new(buffers_returned: Arc<AtomicU64>) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
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
                let status =
                    unsafe { AudioQueueAllocateBuffer(queue, PCM_BYTES as u32, &mut buffer) };
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

        #[cfg(not(target_os = "macos"))]
        {
            let candidates: &[(&str, &[&str])] = &[
                (
                    "pw-cat",
                    &[
                        "--playback",
                        "--raw",
                        "--format=s16",
                        "--rate=48000",
                        "--channels=2",
                        "--latency=20ms",
                        "-",
                    ],
                ),
                (
                    "pacat",
                    &[
                        "--playback",
                        "--raw",
                        "--format=s16le",
                        "--rate=48000",
                        "--channels=2",
                        "--latency-msec=20",
                    ],
                ),
                (
                    "aplay",
                    &[
                        "-q",
                        "-t",
                        "raw",
                        "-f",
                        "S16_LE",
                        "-c",
                        "2",
                        "-r",
                        "48000",
                        "--buffer-size=2048",
                    ],
                ),
            ];
            let mut spawned = None;
            for (bin, args) in candidates {
                if let Ok(mut child) = Command::new(bin)
                    .args(*args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    if let Some(stdin) = child.stdin.take() {
                        tracing::info!(backend = bin, "Linux audio output sink started");
                        spawned = Some((child, Mutex::new(stdin)));
                        break;
                    }
                }
            }
            if spawned.is_none() {
                tracing::warn!(
                    "No PipeWire (pw-cat), PulseAudio (pacat), or ALSA (aplay) sink found; audio will remain silent"
                );
            }
            let (sink_child, sink_stdin) = match spawned {
                Some((c, s)) => (Some(c), Some(s)),
                None => (None, None),
            };
            Ok(Self {
                sink_child,
                sink_stdin,
                buffers_returned,
                dropped: 0,
            })
        }
    }

    pub fn play(&mut self, pcm: &[u8]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
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
            let status = unsafe { AudioQueueStart(self.queue, ptr::null()) };
            if status != 0 {
                bail!("AudioQueueStart after enqueue failed with OSStatus {status}");
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            if let Some(stdin_mutex) = &self.sink_stdin {
                if let Ok(mut stdin) = stdin_mutex.lock() {
                    if stdin.write_all(pcm).is_err() || stdin.flush().is_err() {
                        self.dropped += 1;
                        return Ok(());
                    }
                    self.buffers_returned.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                self.buffers_returned.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

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
        #[cfg(target_os = "macos")]
        unsafe {
            AudioQueueStop(self.queue, true);
            AudioQueueDispose(self.queue, true);
        }
        #[cfg(not(target_os = "macos"))]
        {
            drop(self.sink_stdin.take());
            if let Some(mut child) = self.sink_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn pcm_format() -> AudioStreamBasicDescription {
    pcm_format_with_rate(SAMPLE_RATE)
}

#[cfg(target_os = "macos")]
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
        sync::{atomic::AtomicU64, Arc},
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
