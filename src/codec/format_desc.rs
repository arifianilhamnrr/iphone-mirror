// language: Rust, file: src/codec/format_desc.rs
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Self::Hevc => "hevc",
            Self::H264 => "h264",
        }
    }
}

pub struct FormatDescription {
    pub raw: CMVideoFormatDescriptionRef,
    pub codec: VideoCodec,
    pub annex_b_params: Vec<u8>,
    pub nal_length_size: usize,
    pub dimensions: (usize, usize),
}

// On non-macOS `raw` is always null; on macOS CMVideoFormatDescriptionRef is immutable thread-safe CFType.
unsafe impl Send for FormatDescription {}

impl FormatDescription {
    /// Create format description from VPS, SPS, PPS parameter sets.
    pub fn from_hevc_parameter_sets(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Self> {
        let vps_payload = strip_annex_b_start_code(vps)?;
        let sps_payload = strip_annex_b_start_code(sps)?;
        let pps_payload = strip_annex_b_start_code(pps)?;

        let mut annex_b_params =
            Vec::with_capacity(vps_payload.len() + sps_payload.len() + pps_payload.len() + 12);
        for nal in [vps_payload, sps_payload, pps_payload] {
            annex_b_params.extend_from_slice(&[0, 0, 0, 1]);
            annex_b_params.extend_from_slice(nal);
        }
        let dimensions = parse_hevc_sps_dimensions(sps_payload).unwrap_or((1170, 2532));

        #[cfg(target_os = "macos")]
        let raw = {
            let pointers: [*const u8; 3] = [
                vps_payload.as_ptr(),
                sps_payload.as_ptr(),
                pps_payload.as_ptr(),
            ];
            let sizes: [usize; 3] = [vps_payload.len(), sps_payload.len(), pps_payload.len()];

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
            raw
        };

        #[cfg(not(target_os = "macos"))]
        let raw: CMVideoFormatDescriptionRef = ptr::null_mut();

        Ok(Self {
            raw,
            codec: VideoCodec::Hevc,
            annex_b_params,
            nal_length_size: 4,
            dimensions,
        })
    }

    /// Create format description from H.264 SPS and PPS parameter sets.
    pub fn from_h264_parameter_sets(sps: &[u8], pps: &[u8], nal_length_size: i32) -> Result<Self> {
        let sps_payload = strip_annex_b_start_code(sps)?;
        let pps_payload = strip_annex_b_start_code(pps)?;

        let mut annex_b_params = Vec::with_capacity(sps_payload.len() + pps_payload.len() + 8);
        for nal in [sps_payload, pps_payload] {
            annex_b_params.extend_from_slice(&[0, 0, 0, 1]);
            annex_b_params.extend_from_slice(nal);
        }
        let dimensions = parse_h264_sps_dimensions(sps_payload).unwrap_or((1170, 2532));
        let nal_len = nal_length_size.clamp(1, 4) as usize;

        #[cfg(target_os = "macos")]
        let raw = {
            let pointers: [*const u8; 2] = [sps_payload.as_ptr(), pps_payload.as_ptr()];
            let sizes: [usize; 2] = [sps_payload.len(), pps_payload.len()];

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
            raw
        };

        #[cfg(not(target_os = "macos"))]
        let raw: CMVideoFormatDescriptionRef = ptr::null_mut();

        Ok(Self {
            raw,
            codec: VideoCodec::H264,
            annex_b_params,
            nal_length_size: nal_len,
            dimensions,
        })
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
        bail!("Parameter set is missing its NAL header");
    }
    Ok(payload)
}

fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0_u8;
    for &b in data {
        if zeros == 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros = (zeros + 1).min(2);
        } else {
            zeros = 0;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.data.len() {
            return None;
        }
        let bit_idx = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some(((self.data[byte_idx] >> bit_idx) & 1) as u32)
    }

    fn read_bits(&mut self, count: usize) -> Option<u32> {
        let mut val = 0_u32;
        for _ in 0..count {
            val = (val << 1) | self.read_bit()?;
        }
        Some(val)
    }

    fn skip_bits(&mut self, count: usize) -> Option<()> {
        if self.bit_pos + count > self.data.len() * 8 {
            return None;
        }
        self.bit_pos += count;
        Some(())
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0_usize;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Some(((1_u32 << leading_zeros) - 1) + suffix)
    }

    fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        let val = ((ue + 1) / 2) as i32;
        Some(if ue % 2 == 0 { -val } else { val })
    }
}

/// Extract coded frame dimensions from an H.264 SPS NAL unit (without Annex-B start code).
pub fn parse_h264_sps_dimensions(sps_nal: &[u8]) -> Option<(usize, usize)> {
    let rbsp = remove_emulation_prevention(sps_nal.get(1..)?);
    let mut br = BitReader::new(&rbsp);
    let profile_idc = br.read_bits(8)?;
    br.skip_bits(16)?; // constraint flags + level_idc
    let _sps_id = br.read_ue()?;

    let mut chroma_format_idc = 1_u32;
    let mut separate_colour_plane_flag = 0_u32;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = br.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = br.read_bit()?;
        }
        let _ = br.read_ue()?; // bit_depth_luma_minus8
        let _ = br.read_ue()?; // bit_depth_chroma_minus8
        br.skip_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if br.read_bit()? == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if br.read_bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last_scale = 8_i32;
                    let mut next_scale = 8_i32;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = br.read_se()?;
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        last_scale = if next_scale == 0 { last_scale } else { next_scale };
                    }
                }
            }
        }
    }

    let _ = br.read_ue()?; // log2_max_frame_num_minus4
    let poc_type = br.read_ue()?;
    if poc_type == 0 {
        let _ = br.read_ue()?;
    } else if poc_type == 1 {
        br.skip_bits(1)?;
        let _ = br.read_se()?;
        let _ = br.read_se()?;
        let n = br.read_ue()?;
        for _ in 0..n {
            let _ = br.read_se()?;
        }
    }
    let _ = br.read_ue()?; // max_num_ref_frames
    br.skip_bits(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = br.read_ue()? as usize;
    let pic_height_in_map_units_minus1 = br.read_ue()? as usize;
    let frame_mbs_only_flag = br.read_bit()? as usize;
    if frame_mbs_only_flag == 0 {
        br.skip_bits(1)?; // mb_adaptive_frame_field_flag
    }
    br.skip_bits(1)?; // direct_8x8_inference_flag

    let (crop_l, crop_r, crop_t, crop_b) = if br.read_bit()? == 1 {
        (
            br.read_ue()? as usize,
            br.read_ue()? as usize,
            br.read_ue()? as usize,
            br.read_ue()? as usize,
        )
    } else {
        (0, 0, 0, 0)
    };

    let chroma_array_type = if separate_colour_plane_flag == 0 {
        chroma_format_idc
    } else {
        0
    };
    let (crop_unit_x, crop_unit_y) = if chroma_array_type == 0 {
        (1, 2 - frame_mbs_only_flag)
    } else {
        let sub_width_c = if chroma_format_idc == 1 || chroma_format_idc == 2 {
            2
        } else {
            1
        };
        let sub_height_c = if chroma_format_idc == 1 { 2 } else { 1 };
        (sub_width_c, sub_height_c * (2 - frame_mbs_only_flag))
    };

    let raw_w = (pic_width_in_mbs_minus1 + 1) * 16;
    let raw_h = (2 - frame_mbs_only_flag) * (pic_height_in_map_units_minus1 + 1) * 16;
    let width = raw_w.checked_sub((crop_l + crop_r) * crop_unit_x)?;
    let height = raw_h.checked_sub((crop_t + crop_b) * crop_unit_y)?;
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

/// Extract coded frame dimensions from an HEVC SPS NAL unit (without Annex-B start code).
pub fn parse_hevc_sps_dimensions(sps_nal: &[u8]) -> Option<(usize, usize)> {
    let rbsp = remove_emulation_prevention(sps_nal.get(2..)?);
    let mut br = BitReader::new(&rbsp);
    br.skip_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = br.read_bits(3)? as usize;
    br.skip_bits(1)?; // sps_temporal_id_nesting_flag

    // profile_tier_level(1, max_sub_layers_minus1)
    br.skip_bits(96)?;
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1.min(8) {
        sub_profile[i] = br.read_bit()? == 1;
        sub_level[i] = br.read_bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            br.skip_bits(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1.min(8) {
        if sub_profile[i] {
            br.skip_bits(88)?;
        }
        if sub_level[i] {
            br.skip_bits(8)?;
        }
    }

    let _sps_id = br.read_ue()?;
    let chroma_format_idc = br.read_ue()?;
    if chroma_format_idc == 3 {
        br.skip_bits(1)?;
    }
    let width = br.read_ue()? as usize;
    let height = br.read_ue()? as usize;
    if br.read_bit()? == 1 {
        let crop_l = br.read_ue()? as usize;
        let crop_r = br.read_ue()? as usize;
        let crop_t = br.read_ue()? as usize;
        let crop_b = br.read_ue()? as usize;
        let sub_w = if chroma_format_idc == 1 || chroma_format_idc == 2 {
            2
        } else {
            1
        };
        let sub_h = if chroma_format_idc == 1 { 2 } else { 1 };
        let w = width.checked_sub((crop_l + crop_r) * sub_w)?;
        let h = height.checked_sub((crop_t + crop_b) * sub_h)?;
        return if w > 0 && h > 0 { Some((w, h)) } else { None };
    }

    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::{parse_h264_sps_dimensions, parse_hevc_sps_dimensions, FormatDescription};

    #[test]
    fn parses_avcc_record_and_dimensions() {
        // Minimal Baseline H.264 SPS for 640x480 + PPS
        let sps = [0x67, 0x42, 0x00, 0x1E, 0xE9, 0x00, 0xA0, 0x0F, 0x60];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let mut avcc = vec![1, 0x42, 0x00, 0x1E, 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&pps);

        let (parsed_sps, parsed_pps, nal_len) = FormatDescription::parse_avcc(&avcc).unwrap();
        assert_eq!(parsed_sps, sps);
        assert_eq!(parsed_pps, pps);
        assert_eq!(nal_len, 4);
        assert!(parse_h264_sps_dimensions(&parsed_sps).is_some());
    }

    #[test]
    fn hevc_sps_parser_handles_empty_safely() {
        assert_eq!(parse_hevc_sps_dimensions(&[]), None);
        assert_eq!(parse_hevc_sps_dimensions(&[0x42, 0x01]), None);
    }
}
