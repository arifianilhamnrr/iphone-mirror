// language: Rust, file: src/protocol/negotiator.rs
use anyhow::Result;
use flate2::write::{ZlibDecoder, ZlibEncoder};
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::io::Write;

/// AVConference Media Negotiation Offer for CoreDevice DisplayService
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaNegotiationOffer {
    #[serde(rename = "Version")]
    pub version: u32,
    #[serde(rename = "Codec")]
    pub codec: String, // "H.265" or "HEVC"
    #[serde(rename = "Width")]
    pub width: u32,
    #[serde(rename = "Height")]
    pub height: u32,
    #[serde(rename = "Framerate")]
    pub framerate: u32,
    #[serde(rename = "ReceiverPort")]
    pub receiver_port: u16,
    #[serde(rename = "CipherSuite")]
    pub cipher_suite: u32, // 0 for unencrypted plaintext RTP
}

impl Default for MediaNegotiationOffer {
    fn default() -> Self {
        Self {
            version: 1,
            codec: "H.265".to_string(),
            width: 1170,
            height: 2532,
            framerate: 60,
            receiver_port: 0,
            cipher_suite: 0,
        }
    }
}

impl MediaNegotiationOffer {
    pub fn new(receiver_port: u16, width: u32, height: u32, framerate: u32) -> Self {
        Self {
            version: 1,
            codec: "H.265".to_string(),
            width,
            height,
            framerate,
            receiver_port,
            cipher_suite: 0,
        }
    }

    /// Serialize offer into a plist / binary blob and compress with zlib
    pub fn to_compressed_blob(&self) -> Result<Vec<u8>> {
        let mut plist_buf = Vec::new();
        plist::to_writer_xml(&mut plist_buf, self)?;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&plist_buf)?;
        let compressed = encoder.finish()?;
        Ok(compressed)
    }

    /// Decompress and decode an answer blob from display service
    pub fn decode_answer_blob(blob: &[u8]) -> Result<serde_json::Value> {
        let mut decoder = ZlibDecoder::new(Vec::new());
        decoder.write_all(blob)?;
        let decompressed = decoder.finish()?;

        // Attempt plist parse first, fallback to JSON representation
        let cursor = std::io::Cursor::new(decompressed);
        let val: plist::Value = plist::Value::from_reader(cursor)?;
        let json_val = serde_json::to_value(val)?;
        Ok(json_val)
    }
}
