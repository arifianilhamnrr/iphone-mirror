// language: Rust, file: src/media/rtp.rs
use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            bail!("RTP packet too small: {} bytes (minimum 12 bytes)", data.len());
        }

        let b0 = data[0];
        let version = (b0 >> 6) & 0x03;
        if version != 2 {
            bail!("Unsupported RTP version: {}", version);
        }

        let padding = ((b0 >> 5) & 0x01) == 1;
        let extension = ((b0 >> 4) & 0x01) == 1;
        let csrc_count = (b0 & 0x0F) as usize;

        let b1 = data[1];
        let marker = ((b1 >> 7) & 0x01) == 1;
        let payload_type = b1 & 0x7F;

        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let mut offset = 12 + (csrc_count * 4);
        if data.len() < offset {
            bail!("RTP packet truncated before CSRC list finished");
        }

        if extension {
            if data.len() < offset + 4 {
                bail!("RTP packet truncated in extension header");
            }
            let ext_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4 + (ext_len * 4);
            if data.len() < offset {
                bail!("RTP packet truncated in extension body");
            }
        }

        let mut end = data.len();
        if padding && end > offset {
            let pad_len = data[end - 1] as usize;
            if pad_len > 0 && pad_len <= (end - offset) {
                end -= pad_len;
            }
        }

        let payload = data[offset..end].to_vec();

        Ok(Self {
            version,
            padding,
            extension,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload,
        })
    }
}
