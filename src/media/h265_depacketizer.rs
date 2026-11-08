// language: Rust, file: src/media/h265_depacketizer.rs
use crate::media::rtp::RtpPacket;
use anyhow::{bail, Result};
use tracing::warn;

pub const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

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
    /// Returns Some(complete_nal_with_start_code) when a NAL finishes
    pub fn process_packet(&mut self, pkt: &RtpPacket) -> Result<Option<Vec<u8>>> {
        if pkt.payload.is_empty() {
            return Ok(None);
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
                Ok(Some(nal))
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
                    let reconstructed_hdr1 = ((payload[0] & 0x81)) | (original_nal_type << 1);
                    let reconstructed_hdr2 = payload[1];
                    self.fu_buffer.push(reconstructed_hdr1);
                    self.fu_buffer.push(reconstructed_hdr2);

                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    self.is_assembling_fu = true;
                    Ok(None)
                } else if self.is_assembling_fu {
                    self.fu_buffer.extend_from_slice(&payload[3..]);
                    if e_bit {
                        self.is_assembling_fu = false;
                        let completed_nal = std::mem::take(&mut self.fu_buffer);
                        Ok(Some(completed_nal))
                    } else {
                        Ok(None)
                    }
                } else {
                    // Fragment arrived without start bit, discard
                    Ok(None)
                }
            }

            // Aggregation Packet (AP, Type 48) or other
            _ => {
                warn!(nal_type, "Unhandled H.265 packet type");
                Ok(None)
            }
        }
    }
}
