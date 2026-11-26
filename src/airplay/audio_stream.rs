//! AirPlay RAOP UDP audio receiver (stream type 96: AES-128-CBC + AAC-ELD 44.1kHz).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::airplay::crypto::aes_cbc_decrypt_in_place;
use crate::audio::ring_buffer::PcmRing;
use crate::audio::AacEldDecoder;

const NO_DATA_MARKER: [u8; 4] = [0x00, 0x68, 0x34, 0x00];

/// Bound UDP sockets for AirPlay RAOP audio data & control ports.
pub struct AudioStreamListener {
    pub data_port: u16,
    pub control_port: u16,
    data_socket: UdpSocket,
    control_socket: UdpSocket,
}

impl AudioStreamListener {
    pub async fn bind() -> Result<Self> {
        let data_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind AirPlay audio data UDP socket")?;
        let control_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind AirPlay audio control UDP socket")?;
        let data_port = data_socket.local_addr()?.port();
        let control_port = control_socket.local_addr()?.port();
        Ok(Self {
            data_port,
            control_port,
            data_socket,
            control_socket,
        })
    }

    pub async fn run(
        self,
        aes_key: [u8; 16],
        aes_iv: [u8; 16],
        codec_type: u8,
        ring: Arc<PcmRing>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut decoder = match codec_type {
            8 => match AacEldDecoder::new_with_source_rate(44_100.0) {
                Ok(dec) => Some(dec),
                Err(err) => {
                    warn!("Failed to initialize 44.1kHz AAC-ELD decoder: {err}");
                    None
                }
            },
            other => {
                warn!(codec_type = other, "AirPlay audio codec other than AAC-ELD (ct=8) received; audio may be silent");
                None
            }
        };

        info!(
            data_port = self.data_port,
            control_port = self.control_port,
            codec_type,
            "AirPlay audio UDP receiver started"
        );

        let mut data_buf = vec![0u8; 4096];
        let mut ctrl_buf = vec![0u8; 2048];
        let mut last_seq: Option<u16> = None;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                res = self.data_socket.recv(&mut data_buf) => {
                    match res {
                        Ok(len) if len > 12 => {
                            let pkt = &mut data_buf[..len];
                            let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
                            if !is_newer_seq(seq, last_seq) {
                                continue;
                            }
                            last_seq = Some(seq);
                            decode_and_push_rtp(
                                pkt,
                                &aes_key,
                                &aes_iv,
                                decoder.as_mut(),
                                &ring,
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!("AirPlay audio data recv error: {err}");
                            break;
                        }
                    }
                }
                res = self.control_socket.recv(&mut ctrl_buf) => {
                    match res {
                        Ok(len) if len >= 8 => {
                            let type_c = ctrl_buf[1] & !0x80;
                            // Resent audio packet encapsulated at offset 4
                            if type_c == 0x56 && len > 16 {
                                let pkt = &mut ctrl_buf[4..len];
                                let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
                                if is_newer_seq(seq, last_seq) {
                                    last_seq = Some(seq);
                                    decode_and_push_rtp(
                                        pkt,
                                        &aes_key,
                                        &aes_iv,
                                        decoder.as_mut(),
                                        &ring,
                                    );
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!("AirPlay audio control recv error: {err}");
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn is_newer_seq(seq: u16, last: Option<u16>) -> bool {
    match last {
        None => true,
        Some(prev) => {
            let diff = seq.wrapping_sub(prev);
            diff != 0 && diff < 0x8000
        }
    }
}

fn decode_and_push_rtp(
    packet: &mut [u8],
    aes_key: &[u8; 16],
    aes_iv: &[u8; 16],
    decoder: Option<&mut AacEldDecoder>,
    ring: &PcmRing,
) {
    if packet.len() <= 12 {
        return;
    }
    let payload = &mut packet[12..];
    if payload.len() >= 4 && payload[..4] == NO_DATA_MARKER {
        return;
    }

    aes_cbc_decrypt_in_place(aes_key, aes_iv, payload);

    let Some(dec) = decoder else {
        return;
    };
    match dec.decode(payload) {
        Ok(pcm_bytes) => {
            if pcm_bytes.len() >= 4 {
                let samples: Vec<i16> = pcm_bytes
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                ring.write(&samples);
            }
        }
        Err(err) => {
            debug!("AirPlay AAC-ELD decode failed: {err}");
        }
    }
}
