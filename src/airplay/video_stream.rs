//! AirPlay screen-mirroring H.264 TCP receiver (stream type 110).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::airplay::crypto::Aes128Ctr;
use crate::codec::decoder::{DecodedFrame, HardwareDecoder};
use crate::codec::format_desc::FormatDescription;
use crate::input::coordinate_mapper::DisplayRotation;

/// Receives length-prefixed 128-byte AirPlay mirror headers + payloads on TCP,
/// decrypts VCL video NALs using AES-128-CTR, and feeds them to `HardwareDecoder`.
pub struct VideoStreamListener {
    pub port: u16,
    listener: TcpListener,
}

impl VideoStreamListener {
    pub async fn bind() -> Result<Self> {
        let listener = match TcpListener::bind("[::]:0").await {
            Ok(l) => l,
            Err(_) => TcpListener::bind("0.0.0.0:0")
                .await
                .context("Failed to bind AirPlay video TCP listener")?,
        };
        let port = listener.local_addr()?.port();
        Ok(Self { port, listener })
    }

    pub async fn run(
        self,
        key: [u8; 16],
        iv: [u8; 16],
        frame_tx: mpsc::Sender<DecodedFrame>,
        frame_count: Arc<AtomicU64>,
        orientation_cb: Arc<dyn Fn(DisplayRotation) + Send + Sync>,
    ) {
        match self.listener.accept().await {
            Ok((stream, addr)) => {
                info!(%addr, port = self.port, "AirPlay video stream connected");
                let _ = stream.set_nodelay(true);
                if let Err(err) = handle_video_connection(
                    stream,
                    key,
                    iv,
                    &frame_tx,
                    &frame_count,
                    &orientation_cb,
                )
                .await
                {
                    warn!("AirPlay video connection ended: {err}");
                }
            }
            Err(err) => {
                warn!("AirPlay video accept error: {err}");
            }
        }
    }
}

async fn handle_video_connection(
    mut stream: TcpStream,
    key: [u8; 16],
    iv: [u8; 16],
    frame_tx: &mpsc::Sender<DecodedFrame>,
    frame_count: &Arc<AtomicU64>,
    orientation_cb: &Arc<dyn Fn(DisplayRotation) + Send + Sync>,
) -> Result<()> {
    let mut ctr = Aes128Ctr::new(key, iv);
    let mut header = [0u8; 128];
    let mut decoder: Option<HardwareDecoder> = None;
    let mut last_avcc_config: Vec<u8> = Vec::new();
    let mut last_landscape: Option<bool> = None;
    let mut decode_errors = 0_u32;

    loop {
        if let Err(err) = stream.read_exact(&mut header).await {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                info!("AirPlay video TCP stream closed by iPhone");
                return Ok(());
            }
            return Err(err).context("Failed reading 128-byte AirPlay video header");
        }

        let payload_size =
            u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let payload_type = header[4];

        // Cap at 32 MiB to prevent malformed headers from OOM'ing the receiver.
        if payload_size > 32 * 1024 * 1024 {
            anyhow::bail!("AirPlay video payload too large: {payload_size} bytes");
        }

        let mut payload = vec![0u8; payload_size];
        if payload_size > 0 {
            stream
                .read_exact(&mut payload)
                .await
                .context("Failed reading AirPlay video payload")?;
        }

        match payload_type {
            // Unencrypted H.264 AVCDecoderConfigurationRecord (`avcC`) + display geometry.
            0x01 => {
                let width = f32::from_le_bytes([header[56], header[57], header[58], header[59]]);
                let height = f32::from_le_bytes([header[60], header[61], header[62], header[63]]);
                let src_w = f32::from_le_bytes([header[16], header[17], header[18], header[19]]);
                let src_h = f32::from_le_bytes([header[20], header[21], header[22], header[23]]);
                let eff_w = if width > 0.0 { width } else { src_w };
                let eff_h = if height > 0.0 { height } else { src_h };

                if eff_w > 0.0 && eff_h > 0.0 {
                    let landscape = eff_w > eff_h;
                    if last_landscape != Some(landscape) {
                        last_landscape = Some(landscape);
                        orientation_cb(if landscape {
                            DisplayRotation::LandscapeRight
                        } else {
                            DisplayRotation::Portrait
                        });
                    }
                }

                if payload.is_empty() {
                    warn!("AirPlay received empty codec packet (non-H.264 stream not supported)");
                    continue;
                }
                if payload.get(4..8) == Some(b"hvc1") {
                    warn!("AirPlay received HEVC (hvc1) codec packet; H.264 is expected");
                    continue;
                }

                if payload != last_avcc_config {
                    match FormatDescription::parse_avcc(&payload) {
                        Ok((sps, pps, nal_len_size)) => {
                            match FormatDescription::from_h264_parameter_sets(
                                &sps,
                                &pps,
                                nal_len_size,
                            ) {
                                Ok(fmt) => {
                                    decoder = Some(HardwareDecoder::new(
                                        fmt,
                                        frame_tx.clone(),
                                        Arc::clone(frame_count),
                                    )?);
                                    last_avcc_config = payload;
                                    info!(
                                        width = eff_w as u32,
                                        height = eff_h as u32,
                                        nal_len_size,
                                        "Initialized AirPlay H.264 VideoToolbox decoder"
                                    );
                                }
                                Err(err) => {
                                    warn!("Failed to create H.264 CMVideoFormatDescription: {err}");
                                }
                            }
                        }
                        Err(err) => {
                            warn!("Failed to parse AirPlay avcC record: {err}");
                        }
                    }
                }
            }
            // AES-128-CTR encrypted VCL H.264 frame (4-byte BE length-prefixed AVCC NALUs).
            0x00 => {
                if payload.is_empty() {
                    continue;
                }
                ctr.apply_keystream(&mut payload);
                if let Some(dec) = decoder.as_mut() {
                    if let Err(err) = dec.decode_avcc(&payload) {
                        decode_errors += 1;
                        if decode_errors == 1 || decode_errors % 100 == 0 {
                            warn!(decode_errors, "AirPlay H.264 decode error: {err}");
                        }
                    } else {
                        let count = frame_count.load(Ordering::Relaxed);
                        if count > 0 && (count == 1 || count % 300 == 0) {
                            info!(frame = count, "AirPlay decoded H.264 frame");
                        }
                    }
                }
            }
            // Heartbeat (0x02) or streaming report (0x05)
            0x02 | 0x05 => {}
            other => {
                debug!(
                    payload_type = other,
                    payload_size, "Ignoring unknown AirPlay video packet type"
                );
            }
        }
    }
}
