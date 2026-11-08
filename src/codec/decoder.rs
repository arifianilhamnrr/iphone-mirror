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

    extern "C" fn decode_callback(
        _output_ref_con: *mut std::ffi::c_void,
        _source_frame_ref_con: *mut std::ffi::c_void,
        status: OSStatus,
        _info_flags: u32,
        _image_buffer: CVPixelBufferRef,
        _pts: u64,
        _duration: u64,
    ) {
        if status != 0 {
            tracing::warn!(status, "Decode frame callback returned error");
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
