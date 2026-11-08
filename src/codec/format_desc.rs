// language: Rust, file: src/codec/format_desc.rs
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
use std::ptr;

pub struct FormatDescription {
    pub raw: CMVideoFormatDescriptionRef,
}

impl FormatDescription {
    /// Create format description from VPS, SPS, PPS parameter sets
    pub fn from_hevc_parameter_sets(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let pointers: [*const u8; 3] = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
            let sizes: [usize; 3] = [vps.len(), sps.len(), pps.len()];

            let mut raw: CMVideoFormatDescriptionRef = ptr::null_mut();
            let status = unsafe {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    ptr::null_mut(),
                    3,
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4, // 4-byte NAL header length (AVCC)
                    ptr::null_mut(),
                    &mut raw,
                )
            };

            if status != 0 || raw.is_null() {
                bail!("CMVideoFormatDescriptionCreateFromHEVCParameterSets failed with code {}", status);
            }

            Ok(Self { raw })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (vps, sps, pps);
            bail!("Hardware VideoToolbox decoding is only available on macOS");
        }
    }
}
