// language: Rust, file: src/codec/decoder.rs
use crate::codec::format_desc::FormatDescription;
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
use std::ptr;
use tracing::info;

pub struct HardwareDecoder {
    session: VTDecompressionSessionRef,
    _format_desc: FormatDescription,
}

impl HardwareDecoder {
    pub fn new(format_desc: FormatDescription) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut session: VTDecompressionSessionRef = ptr::null_mut();
            let callback_record = VTDecompressionOutputCallbackRecord {
                decompression_output_callback: Some(Self::decode_callback),
                decompression_output_ref_con: ptr::null_mut(),
            };

            let status = unsafe {
                VTDecompressionSessionCreate(
                    ptr::null_mut(),
                    format_desc.raw,
                    ptr::null_mut(),
                    ptr::null_mut(), // Destination attributes (Metal compatible)
                    &callback_record,
                    &mut session,
                )
            };

            if status != 0 || session.is_null() {
                bail!("VTDecompressionSessionCreate failed with OSStatus {}", status);
            }

            info!("Hardware VideoToolbox decompression session initialized");
            Ok(Self {
                session,
                _format_desc: format_desc,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = format_desc;
            bail!("Hardware VideoToolbox decoding is only available on macOS");
        }
    }

    /// Feed complete Annex-B NAL unit (with 4-byte start code) to hardware decoder
    pub fn decode_nal(&mut self, nal: &[u8]) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            if nal.len() <= 4 {
                return Ok(());
            }

            // Convert Annex-B (00 00 00 01) to AVCC (4-byte big-endian length)
            let mut avcc = nal.to_vec();
            let payload_len = (avcc.len() - 4) as u32;
            avcc[0..4].copy_from_slice(&payload_len.to_be_bytes());

            let mut block_buffer: CMBlockBufferRef = ptr::null_mut();
            let status = unsafe {
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null_mut(),
                    avcc.as_mut_ptr() as *mut std::ffi::c_void,
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
                bail!("CMBlockBufferCreateWithMemoryBlock failed with code {}", status);
            }

            let mut sample_buffer: CMSampleBufferRef = ptr::null_mut();
            let sample_size = avcc.len();
            let status = unsafe {
                CMSampleBufferCreateReady(
                    ptr::null_mut(),
                    block_buffer,
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
                bail!("CMSampleBufferCreateReady failed with code {}", status);
            }

            let mut info_flags: u32 = 0;
            let status = unsafe {
                VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample_buffer,
                    0,
                    ptr::null_mut(),
                    &mut info_flags,
                )
            };

            if status != 0 {
                tracing::debug!(status, "VTDecompressionSessionDecodeFrame warning");
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

impl Drop for HardwareDecoder {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
            }
        }
    }
}
