// language: Rust, file: src/codec/decoder.rs
use crate::codec::format_desc::FormatDescription;
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::mpsc;
use tracing::info;

#[cfg(not(target_os = "macos"))]
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::AtomicBool,
    thread::JoinHandle,
};

#[derive(Debug, Clone)]
pub struct CpuFrame {
    pub width: usize,
    pub height: usize,
    pub nv12: Vec<u8>,
    pub full_range: bool,
}

/// A retained decoded output frame (VideoToolbox `CVPixelBuffer` on macOS, or NV12 `CpuFrame` on Linux).
pub struct DecodedFrame {
    pixel_buffer: CVPixelBufferRef,
    cpu_frame: Option<Arc<CpuFrame>>,
    pub presentation_time: CMTime,
    pub duration: CMTime,
}

// CVPixelBuffer is an IOSurface-backed Core Foundation object designed to cross threads.
// CpuFrame is wrapped in Arc and contains plain bytes.
unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    pub fn from_nv12(width: usize, height: usize, nv12: Vec<u8>, full_range: bool) -> Self {
        Self {
            pixel_buffer: ptr::null_mut(),
            cpu_frame: Some(Arc::new(CpuFrame {
                width,
                height,
                nv12,
                full_range,
            })),
            presentation_time: CMTime::default(),
            duration: CMTime::default(),
        }
    }

    pub fn cpu_frame(&self) -> Option<&Arc<CpuFrame>> {
        self.cpu_frame.as_ref()
    }

    pub fn width(&self) -> usize {
        if let Some(frame) = &self.cpu_frame {
            return frame.width;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                return CVPixelBufferGetWidth(self.pixel_buffer);
            }
        }
        0
    }

    pub fn height(&self) -> usize {
        if let Some(frame) = &self.cpu_frame {
            return frame.height;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                return CVPixelBufferGetHeight(self.pixel_buffer);
            }
        }
        0
    }

    pub fn as_ptr(&self) -> CVPixelBufferRef {
        self.pixel_buffer
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                CVPixelBufferRelease(self.pixel_buffer);
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct CallbackContext {
    frame_tx: mpsc::Sender<DecodedFrame>,
    frame_count: Arc<AtomicU64>,
}

pub struct HardwareDecoder {
    #[cfg(target_os = "macos")]
    session: VTDecompressionSessionRef,
    #[cfg(target_os = "macos")]
    _callback_context: Box<CallbackContext>,
    #[cfg(not(target_os = "macos"))]
    stdin: Option<ChildStdin>,
    #[cfg(not(target_os = "macos"))]
    child: Child,
    #[cfg(not(target_os = "macos"))]
    stop: Arc<AtomicBool>,
    #[cfg(not(target_os = "macos"))]
    reader_thread: Option<JoinHandle<()>>,
    _format_desc: FormatDescription,
}

// VTDecompressionSession accepts decode submissions from a worker task and owns
// its callback synchronization. On Linux, Child + ChildStdin are owned exclusively by this task.
unsafe impl Send for HardwareDecoder {}

impl HardwareDecoder {
    #[cfg(target_os = "macos")]
    extern "C" fn decode_callback(
        decompression_output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        image_buffer: CVPixelBufferRef,
        presentation_time: CMTime,
        duration: CMTime,
    ) {
        if status != 0 || image_buffer.is_null() {
            tracing::warn!(
                status,
                null_buf = image_buffer.is_null(),
                "VideoToolbox output callback reported an error"
            );
            return;
        }

        let context = unsafe { &*(decompression_output_ref_con as *const CallbackContext) };
        context.frame_count.fetch_add(1, Ordering::Relaxed);
        let retained = unsafe { CVPixelBufferRetain(image_buffer) };
        let frame = DecodedFrame {
            pixel_buffer: retained,
            cpu_frame: None,
            presentation_time,
            duration,
        };

        if let Err(error) = context.frame_tx.try_send(frame) {
            tracing::debug!(%error, "Dropping decoded frame because the renderer is behind");
        }
    }

    pub fn new(
        format_desc: FormatDescription,
        frame_tx: mpsc::Sender<DecodedFrame>,
        frame_count: Arc<AtomicU64>,
    ) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut callback_context = Box::new(CallbackContext {
                frame_tx,
                frame_count,
            });
            let callback_record = VTDecompressionOutputCallbackRecord {
                decompression_output_callback: Some(Self::decode_callback),
                decompression_output_ref_con: (&mut *callback_context as *mut CallbackContext)
                    .cast(),
            };
            let mut session: VTDecompressionSessionRef = ptr::null_mut();

            let status = unsafe {
                VTDecompressionSessionCreate(
                    ptr::null_mut(),
                    format_desc.raw,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &callback_record,
                    &mut session,
                )
            };

            if status != 0 || session.is_null() {
                bail!("VTDecompressionSessionCreate failed with OSStatus {status}");
            }

            info!("Hardware VideoToolbox decompression session initialized");
            Ok(Self {
                session,
                _format_desc: format_desc,
                _callback_context: callback_context,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let (width, height) = format_desc.dimensions;
            if width == 0 || height == 0 {
                bail!("Invalid decoded frame dimensions {width}x{height}");
            }
            let frame_bytes = width * height * 3 / 2;
            let codec_fmt = format_desc.codec.ffmpeg_format();

            let mut child = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-hwaccel",
                    "auto",
                    "-fflags",
                    "nobuffer",
                    "-flags",
                    "low_delay",
                    "-probesize",
                    "32",
                    "-analyzeduration",
                    "0",
                    "-f",
                    codec_fmt,
                    "-i",
                    "pipe:0",
                    "-vf",
                    &format!("scale={width}:{height}"),
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "nv12",
                    "pipe:1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|err| {
                    anyhow::anyhow!(
                        "Failed to start ffmpeg video decoder ({codec_fmt}): {err}. Install ffmpeg for Linux video decoding."
                    )
                })?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin pipe unavailable"))?;
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg stdout pipe unavailable"))?;

            if !format_desc.annex_b_params.is_empty() {
                stdin.write_all(&format_desc.annex_b_params)?;
                stdin.flush()?;
            }

            let stop = Arc::new(AtomicBool::new(false));
            let stop_reader = stop.clone();
            let reader_thread = std::thread::Builder::new()
                .name("linux-video-decoder".to_string())
                .spawn(move || {
                    while !stop_reader.load(Ordering::Acquire) {
                        let mut buf = vec![0_u8; frame_bytes];
                        if stdout.read_exact(&mut buf).is_err() {
                            break;
                        }
                        frame_count.fetch_add(1, Ordering::Relaxed);
                        let frame = DecodedFrame::from_nv12(width, height, buf, false);
                        if let Err(error) = frame_tx.try_send(frame) {
                            tracing::debug!(%error, "Dropping decoded frame because the renderer is behind");
                        }
                    }
                })?;

            info!(
                width,
                height,
                codec = codec_fmt,
                "Linux ffmpeg video decoder initialized"
            );
            Ok(Self {
                stdin: Some(stdin),
                child,
                stop,
                reader_thread: Some(reader_thread),
                _format_desc: format_desc,
            })
        }
    }

    /// Feed one complete Annex-B NAL unit to the decoder.
    pub fn decode_nal(&mut self, nal: &[u8]) -> Result<()> {
        let payload = annex_b_payload(nal)?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            anyhow::anyhow!("HEVC NAL is too large for its 4-byte AVCC length prefix")
        })?;
        let mut avcc = Vec::with_capacity(payload.len() + 4);
        avcc.extend_from_slice(&payload_len.to_be_bytes());
        avcc.extend_from_slice(payload);
        self.decode_avcc(&avcc)
    }

    /// Feed an AVCC length-prefixed access unit directly to the decoder.
    pub fn decode_avcc(&mut self, avcc: &[u8]) -> Result<()> {
        if avcc.is_empty() {
            return Ok(());
        }

        #[cfg(target_os = "macos")]
        {
            let mut block_buffer: CMBlockBufferRef = ptr::null_mut();
            let status = unsafe {
                // A null memory block asks CoreMedia to allocate storage it owns.
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    avcc.len(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    avcc.len(),
                    0,
                    &mut block_buffer,
                )
            };
            if status != 0 || block_buffer.is_null() {
                bail!("CMBlockBufferCreateWithMemoryBlock failed with OSStatus {status}");
            }
            let block_buffer = CfGuard::new(block_buffer);

            let status = unsafe {
                CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), block_buffer.raw, 0, avcc.len())
            };
            if status != 0 {
                bail!("CMBlockBufferReplaceDataBytes failed with OSStatus {status}");
            }

            let mut sample_buffer: CMSampleBufferRef = ptr::null_mut();
            let sample_size = avcc.len();
            let status = unsafe {
                CMSampleBufferCreateReady(
                    ptr::null_mut(),
                    block_buffer.raw,
                    self._format_desc.raw,
                    1,
                    0,
                    ptr::null(),
                    1,
                    &sample_size,
                    &mut sample_buffer,
                )
            };
            if status != 0 || sample_buffer.is_null() {
                bail!("CMSampleBufferCreateReady failed with OSStatus {status}");
            }
            let sample_buffer = CfGuard::new(sample_buffer);

            let mut info_flags = 0;
            let status = unsafe {
                VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample_buffer.raw,
                    0,
                    ptr::null_mut(),
                    &mut info_flags,
                )
            };
            if status != 0 {
                bail!("VTDecompressionSessionDecodeFrame failed with OSStatus {status}");
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let annex_b = avcc_to_annex_b(avcc, self._format_desc.nal_length_size);
            if annex_b.is_empty() {
                return Ok(());
            }
            if let Some(stdin) = self.stdin.as_mut() {
                stdin.write_all(&annex_b)?;
                stdin.flush()?;
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn avcc_to_annex_b(avcc: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut offset = 0;
    let n = nal_length_size.clamp(1, 4);
    while offset + n <= avcc.len() {
        let mut nal_len = 0_usize;
        for &b in &avcc[offset..offset + n] {
            nal_len = (nal_len << 8) | (b as usize);
        }
        offset += n;
        if nal_len == 0 || offset + nal_len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[offset..offset + nal_len]);
        offset += nal_len;
    }
    out
}

fn annex_b_payload(nal: &[u8]) -> Result<&[u8]> {
    let payload = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        bail!("Expected an Annex-B NAL start code");
    };
    if payload.len() < 2 {
        bail!("NAL is missing its two-byte header");
    }
    Ok(payload)
}

#[cfg(target_os = "macos")]
struct CfGuard<T> {
    raw: *mut T,
}

#[cfg(target_os = "macos")]
impl<T> CfGuard<T> {
    fn new(raw: *mut T) -> Self {
        Self { raw }
    }
}

#[cfg(target_os = "macos")]
impl<T> Drop for CfGuard<T> {
    fn drop(&mut self) {
        unsafe { CFRelease(self.raw.cast()) }
    }
}

impl Drop for HardwareDecoder {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.session.is_null() {
                let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session.cast());
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.stop.store(true, Ordering::Release);
            drop(self.stdin.take());
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(handle) = self.reader_thread.take() {
                let _ = handle.join();
            }
        }
    }
}
