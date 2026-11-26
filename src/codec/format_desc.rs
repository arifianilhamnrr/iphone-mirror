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
            let vps = strip_annex_b_start_code(vps)?;
            let sps = strip_annex_b_start_code(sps)?;
            let pps = strip_annex_b_start_code(pps)?;
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
                bail!(
                    "CMVideoFormatDescriptionCreateFromHEVCParameterSets failed with code {}",
                    status
                );
            }

            Ok(Self { raw })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (vps, sps, pps);
            bail!("Hardware VideoToolbox decoding is only available on macOS");
        }
    }

    /// Create format description from H.264 SPS and PPS parameter sets.
    pub fn from_h264_parameter_sets(sps: &[u8], pps: &[u8], nal_length_size: i32) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let sps = strip_annex_b_start_code(sps)?;
            let pps = strip_annex_b_start_code(pps)?;
            let pointers: [*const u8; 2] = [sps.as_ptr(), pps.as_ptr()];
            let sizes: [usize; 2] = [sps.len(), pps.len()];

            let mut raw: CMVideoFormatDescriptionRef = ptr::null_mut();
            let status = unsafe {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null_mut(),
                    2,
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    nal_length_size,
                    &mut raw,
                )
            };

            if status != 0 || raw.is_null() {
                bail!(
                    "CMVideoFormatDescriptionCreateFromH264ParameterSets failed with code {}",
                    status
                );
            }

            Ok(Self { raw })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (sps, pps, nal_length_size);
            bail!("Hardware VideoToolbox decoding is only available on macOS");
        }
    }

    /// Parse an ISO 14496-15 `avcC` configuration record into `(sps, pps, nal_length_size)`.
    pub fn parse_avcc(avcc: &[u8]) -> Result<(Vec<u8>, Vec<u8>, i32)> {
        if avcc.len() < 7 || avcc[0] != 1 {
            bail!("Invalid avcC record");
        }
        let nal_length_size = ((avcc[4] & 0x03) + 1) as i32;
        let num_sps = (avcc[5] & 0x1F) as usize;
        let mut offset = 6;
        let mut sps = None;
        for _ in 0..num_sps {
            if offset + 2 > avcc.len() {
                bail!("Truncated avcC SPS length");
            }
            let len = u16::from_be_bytes([avcc[offset], avcc[offset + 1]]) as usize;
            offset += 2;
            if offset + len > avcc.len() {
                bail!("Truncated avcC SPS payload");
            }
            if sps.is_none() && len > 0 {
                sps = Some(avcc[offset..offset + len].to_vec());
            }
            offset += len;
        }
        if offset >= avcc.len() {
            bail!("Missing avcC PPS count");
        }
        let num_pps = avcc[offset] as usize;
        offset += 1;
        let mut pps = None;
        for _ in 0..num_pps {
            if offset + 2 > avcc.len() {
                bail!("Truncated avcC PPS length");
            }
            let len = u16::from_be_bytes([avcc[offset], avcc[offset + 1]]) as usize;
            offset += 2;
            if offset + len > avcc.len() {
                bail!("Truncated avcC PPS payload");
            }
            if pps.is_none() && len > 0 {
                pps = Some(avcc[offset..offset + len].to_vec());
            }
            offset += len;
        }
        let sps = sps.ok_or_else(|| anyhow::anyhow!("avcC contained no SPS"))?;
        let pps = pps.ok_or_else(|| anyhow::anyhow!("avcC contained no PPS"))?;
        Ok((sps, pps, nal_length_size))
    }
}

fn strip_annex_b_start_code(nal: &[u8]) -> Result<&[u8]> {
    let payload = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    };

    if payload.len() < 2 {
        bail!("HEVC parameter set is missing its NAL header");
    }
    Ok(payload)
}

impl Drop for FormatDescription {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.raw.is_null() {
                CFRelease(self.raw.cast());
            }
        }
    }
}
