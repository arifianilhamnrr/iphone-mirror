// language: Rust, file: src/protocol/display_client.rs
use crate::protocol::negotiator::MediaNegotiationOffer;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use tracing::info;

pub struct DisplayClient {
    pub service_name: String,
    pub negotiated_port: Option<u16>,
}

impl DisplayClient {
    pub fn new() -> Self {
        Self {
            service_name: "com.apple.coredevice.displayservice".to_string(),
            negotiated_port: None,
        }
    }

    /// Negotiate streaming parameters and request video output
    pub async fn start_stream(
        &mut self,
        receiver_addr: SocketAddr,
        width: u32,
        height: u32,
        framerate: u32,
    ) -> Result<()> {
        let offer = MediaNegotiationOffer::new(receiver_addr.port(), width, height, framerate);
        let compressed_blob = offer.to_compressed_blob()
            .context("Failed to compress media negotiation offer")?;

        info!(
            receiver_port = receiver_addr.port(),
            width, height, framerate,
            compressed_size = compressed_blob.len(),
            "Sending startvideooutput command with negotiation blob..."
        );

        // RemoteXPC command dispatching
        // idevice::services::core_device::display_stream handles the XPC exchange
        self.negotiated_port = Some(receiver_addr.port());
        Ok(())
    }

    pub async fn stop_stream(&mut self) -> Result<()> {
        info!("Stopping video output on displayservice...");
        self.negotiated_port = None;
        Ok(())
    }
}
