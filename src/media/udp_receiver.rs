// language: Rust, file: src/media/udp_receiver.rs
use crate::media::h265_depacketizer::H265Depacketizer;
use crate::media::rtp::RtpPacket;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

pub struct UdpMediaReceiver {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
}

impl UdpMediaReceiver {
    /// Bind ephemeral port on localhost with 4MB receive buffer
    pub async fn bind() -> Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("Failed to bind UDP socket")?;

        let local_addr = socket.local_addr()?;
        info!(local_addr = %local_addr, "UDP Media Receiver socket bound successfully");

        Ok(Self {
            socket: Arc::new(socket),
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Spawns background worker receiving packets and forwarding complete NAL units
    pub fn start_worker(self, nal_tx: mpsc::Sender<Vec<u8>>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let mut depacketizer = H265Depacketizer::new();

            loop {
                match self.socket.recv_from(&mut buf).await {
                    Ok((len, _from)) => match RtpPacket::parse(&buf[..len]) {
                        Ok(pkt) if !RtpPacket::is_rtcp(&buf[..len]) => {
                            match depacketizer.process_packet(&pkt) {
                                Ok(nals) => {
                                    for nal in nals {
                                        if nal_tx.send(nal).await.is_err() {
                                            debug!("Receiver channel closed, stopping worker");
                                            return;
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!(error = %e, "Failed to depacketize packet");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            debug!(error = %e, "Corrupt RTP packet received");
                        }
                    },
                    Err(e) => {
                        error!(error = %e, "UDP socket read error");
                        break;
                    }
                }
            }
        })
    }
}
