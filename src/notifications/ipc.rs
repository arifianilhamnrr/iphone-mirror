use crate::notifications::{IosNotification, NotificationSettings};
use serde::{Deserialize, Serialize};

/// Messages sent from the OS host (Swift macOS Launcher or Linux loop) into the Rust notification daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToDaemonMessage {
    BleConnected { peripheral_name: String },
    BleDisconnected { reason: String },
    BleNotificationSource { bytes_b64: String },
    BleDataSource { bytes_b64: String },
    PerformAction { uid: u32, positive: bool },
    SetMirrorActive { active: bool },
    SendTestNotification,
    UpdateSettings { settings: NotificationSettings },
}

/// Messages sent from the Rust notification daemon back to the OS host / UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToHostMessage {
    WriteControlPoint {
        bytes_b64: String,
    },
    DeliverNotification {
        notification: IosNotification,
    },
    RecordHistoryOnly {
        notification: IosNotification,
    },
    RetractNotification {
        uid: u32,
    },
    LaunchApp {
        bundle_id: String,
    },
    RelayReady {
        port: u16,
        token: String,
        #[serde(default)]
        lan_url: Option<String>,
    },
    Status {
        message: String,
    },
}

/// Minimal dependency-free base64 encoder/decoder for small BLE GATT payloads over JSON-line IPC.
pub fn encode_b64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn decode_b64(input: &str) -> anyhow::Result<Vec<u8>> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        anyhow::bail!("Invalid base64 length");
    }
    let val = |c: u8| -> anyhow::Result<u8> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => anyhow::bail!("Invalid base64 byte {c}"),
        }
    };
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks_exact(4) {
        let c0 = val(chunk[0])?;
        let c1 = val(chunk[1])?;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        let c2 = if pad2 { 0 } else { val(chunk[2])? };
        let c3 = if pad3 { 0 } else { val(chunk[3])? };
        let n = ((c0 as u32) << 18) | ((c1 as u32) << 12) | ((c2 as u32) << 6) | (c3 as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if !pad2 {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if !pad3 {
            out.push((n & 0xFF) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        let payloads: &[&[u8]] = &[b"", b"A", b"AB", b"ANCS", &[0x00, 0xff, 0x10, 0x80, 0x42]];
        for &p in payloads {
            let enc = encode_b64(p);
            let dec = decode_b64(&enc).expect("valid base64");
            assert_eq!(dec.as_slice(), p);
        }
    }
}
