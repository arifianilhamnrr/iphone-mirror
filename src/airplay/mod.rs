//! Optional AirPlay H.264 Screen Mirroring Receiver mode.

pub mod audio_stream;
pub mod bonjour;
pub mod crypto;
pub mod playfair;
pub mod rtsp;
pub mod video_stream;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use anyhow::Result;
use rand::RngCore;
use tokio::sync::{mpsc, watch};

use crate::airplay::bonjour::BonjourRegistration;
use crate::airplay::rtsp::{AirPlayCallbacks, RtspServer};
use crate::audio::ring_buffer::PcmRing;
use crate::codec::decoder::DecodedFrame;
use crate::input::coordinate_mapper::DisplayRotation;

pub struct AirPlayReceiver {
    pub port: u16,
    pub receiver_name: String,
    _bonjour: BonjourRegistration,
    shutdown_tx: watch::Sender<bool>,
}

impl AirPlayReceiver {
    pub async fn start(
        receiver_name: String,
        frame_tx: mpsc::Sender<DecodedFrame>,
        frame_count: Arc<AtomicU64>,
        audio_ring: Arc<PcmRing>,
        orientation_cb: Arc<dyn Fn(DisplayRotation) + Send + Sync>,
    ) -> Result<Self> {
        let server = RtspServer::bind().await?;
        let port = server.port;
        let pk_hex = server.public_key_hex();

        let mut mac = [0u8; 6];
        rand::thread_rng().fill_bytes(&mut mac);
        // Locally administered unicast MAC address
        mac[0] = (mac[0] & 0xfe) | 0x02;
        let device_id = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        let bonjour = BonjourRegistration::register(&receiver_name, port, &device_id, &pk_hex)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let callbacks = Arc::new(AirPlayCallbacks {
            frame_tx,
            frame_count,
            audio_ring,
            orientation_cb,
            receiver_name: receiver_name.clone(),
            device_id,
        });

        tokio::spawn(async move {
            server.serve(callbacks, shutdown_rx).await;
        });

        Ok(Self {
            port,
            receiver_name,
            _bonjour: bonjour,
            shutdown_tx,
        })
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}
