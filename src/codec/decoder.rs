// language: Rust, file: src/codec/decoder.rs
use crate::codec::format_desc::FormatDescription;
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
use std::{ffi::c_void, ptr};
use tokio::sync::mpsc;
use tracing::info;

/// A retained VideoToolbox output frame. The pixel buffer is released on drop.
pub struct DecodedFrame {
    pixel_buffer: CVPixelBufferRef,
    pub presentation_time: CMTime,
    pub duration: CMTime,
}

// CVPixelBuffer is an IOSurface-backed Core Foundation object designed to cross threads.
unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    pub fn width(&self) -> usize {
        #[cfg(target_os = "macos")]
        unsafe {
            CVPixelBufferGetWidth(self.pixel_buffer)
        }
        #[cfg(not(target_os = "macos"))]
        0
    }

    pub fn height(&self) -> usize {
        #[cfg(target_os = "macos")]
        unsafe {
            CVPixelBufferGetHeight(self.pixel_buffer)
        }
        #[cfg(not(target_os = "macos"))]
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
            CVPixelBufferRelease(self.pixel_buffer);
        }
    }
}

struct CallbackContext {
    frame_tx: mpsc::Sender<DecodedFrame>,
}

pub struct HardwareDecoder {
    session: VTDecompressionSessionRef,
    _format_desc: FormatDescription,
    _callback_context: Box<CallbackContext>,
}

// VTDecompressionSession accepts decode submissions from a worker task and owns
// its callback synchronization. This type is moved, never shared concurrently.
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
            tracing::debug!(status, "VideoToolbox output callback reported an error");
            return;
        }

        let context = unsafe { &*(decompression_output_ref_con as *const CallbackContext) };
        let retained = unsafe { CVPixelBufferRetain(image_buffer) };
        let frame = DecodedFrame {
            pixel_buffer: retained,
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
    ) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut callback_context = Box::new(CallbackContext { frame_tx });
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
            let _ = (format_desc, frame_tx);
            bail!("Hardware VideoToolbox decoding is only available on macOS");
        }
    }

    /// Feed one complete Annex-B NAL unit to VideoToolbox.
    pub fn decode_nal(&mut self, nal: &[u8]) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let payload = annex_b_payload(nal)?;
            let payload_len = u32::try_from(payload.len()).map_err(|_| {
                anyhow::anyhow!("HEVC NAL is too large for its 4-byte AVCC length prefix")
            })?;
            let mut avcc = Vec::with_capacity(payload.len() + 4);
            avcc.extend_from_slice(&payload_len.to_be_bytes());
            avcc.extend_from_slice(payload);

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
            let _ = nal;
            Ok(())
        }
    }
}

fn annex_b_payload(nal: &[u8]) -> Result<&[u8]> {
    let payload = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        bail!("Expected an Annex-B HEVC NAL start code");
    };
    if payload.len() < 2 {
        bail!("HEVC NAL is missing its two-byte header");
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
    }
}
