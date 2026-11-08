// language: Rust, file: src/media/h265_depacketizer.rs
use crate::media::rtp::RtpPacket;
use anyhow::{bail, Result};
use tracing::warn;

pub const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
const MAX_NAL_SIZE: usize = 16 * 1024 * 1024;

pub struct H265Depacketizer {
    fu_buffer: Vec<u8>,
    is_assembling_fu: bool,
    expected_seq: Option<u16>,
}

impl H265Depacketizer {
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::with_capacity(65536),
            is_assembling_fu: false,
            expected_seq: None,
        }
    }

    /// Process incoming RTP packet per RFC 7798
    /// Returns every complete Annex-B NAL carried by the packet.
    pub fn process_packet(&mut self, pkt: &RtpPacket) -> Result<Vec<Vec<u8>>> {
        if pkt.payload.is_empty() {
            return Ok(Vec::new());
        }

        // Sequence continuity check
        if let Some(expected) = self.expected_seq {
            if pkt.sequence_number != expected {
                warn!(
                    received = pkt.sequence_number,
                    expected, "RTP sequence gap detected, discarding partial FU buffer"
                );
                self.fu_buffer.clear();
                self.is_assembling_fu = false;
            }
        }
        self.expected_seq = Some(pkt.sequence_number.wrapping_add(1));

        let payload = &pkt.payload;
        let payload_hdr = payload[0];
        let nal_type = (payload_hdr >> 1) & 0x3F;

        match nal_type {
            // Single NAL Unit Packet (Types 0..47 except 48, 49)
            0..=47 => {
                let mut nal = Vec::with_capacity(START_CODE.len() + payload.len());
                nal.extend_from_slice(&START_CODE);
                nal.extend_from_slice(payload);
                Ok(vec![nal])
            }

            // Aggregation Packet (AP, Type 48): 2-byte AP header followed by
            // repeated 16-bit lengths and complete NAL units.
            48 => {
                if payload.len() < 4 {
                    bail!("H.265 aggregation packet is too short");
                }
                let mut offset = 2;
                let mut nals = Vec::new();
                while offset < payload.len() {
                    if payload.len() - offset < 2 {
                        bail!("Truncated H.265 aggregation-unit length");
                    }
                    let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                    offset += 2;
                    if size < 2 || payload.len() - offset < size {
                        bail!("Invalid H.265 aggregation-unit size: {size}");
                    }
                    let mut nal = Vec::with_capacity(START_CODE.len() + size);
                    nal.extend_from_slice(&START_CODE);
                    nal.extend_from_slice(&payload[offset..offset + size]);
                    nals.push(nal);
                    offset += size;
                }
                Ok(nals)
            }

            // Fragmentation Unit (FU, Type 49)
            49 => {
                if payload.len() < 3 {
                    bail!("FU packet too short (expected >= 3 bytes)");
                }

                let fu_header = payload[2];
                let s_bit = (fu_header >> 7) & 0x01 == 1;
                let e_bit = (fu_header >> 6) & 0x01 == 1;
                let original_nal_type = fu_header & 0x3F;

                if s_bit {
                    // Start of fragmented NAL unit
                    self.fu_buffer.clear();
                    self.fu_buffer.extend_from_slice(&START_CODE);

                    // Reconstruct reconstructed NAL header (2 bytes)
                    let reconstructed_hdr1 = (payload[0] & 0x81) | (original_nal_type << 1);
                    let reconstructed_hdr2 = payload[1];
                    self.fu_buffer.push(reconstructed_hdr1);
                    self.fu_buffer.push(reconstructed_hdr2);

                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    self.is_assembling_fu = true;
                    Ok(Vec::new())
                } else if self.is_assembling_fu {
                    if self.fu_buffer.len() + payload.len().saturating_sub(3) > MAX_NAL_SIZE {
                        self.fu_buffer.clear();
                        self.is_assembling_fu = false;
                        bail!("Fragmented H.265 NAL exceeds the 16 MiB safety limit");
                    }
                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    if e_bit {
                        self.is_assembling_fu = false;
                        let completed_nal = std::mem::take(&mut self.fu_buffer);
                        Ok(vec![completed_nal])
                    } else {
                        Ok(Vec::new())
                    }
                } else {
                    // Fragment arrived without start bit, discard
                    Ok(Vec::new())
                }
            }

            // PACI (type 50) or unsupported packet type.
            _ => {
                warn!(nal_type, "Unhandled H.265 packet type");
                Ok(Vec::new())
            }
        }
    }
}

impl Default for H265Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}
