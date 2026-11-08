// language: Rust, file: src/transport/rsd_tunnel.rs, target: macOS / Linux
use anyhow::{Context, Result};
use idevice::{
    core_device_proxy::CoreDeviceProxy,
    provider::IdeviceProvider,
    rsd::RsdHandshake,
    tcp::stream::AdapterStream,
};
use tracing::{info, warn};

pub const DISPLAY_SERVICE: &str = "com.apple.coredevice.displayservice";

pub struct RsdTunnel {
    pub stream: AdapterStream,
    pub services: Vec<String>,
}

impl RsdTunnel {
    /// Connect to device proxy, start software tunnel, and perform RSD handshake
    pub async fn connect(provider: Box<dyn IdeviceProvider>) -> Result<Self> {
        info!("Connecting to CoreDeviceProxy...");
        let proxy = CoreDeviceProxy::connect(&*provider)
            .await
            .context("Failed to connect to CoreDeviceProxy")?;

        let rsd_port = proxy.tunnel_info().server_rsd_port;
        info!(rsd_port, "Starting software tunnel for RSD...");

        let mut adapter = proxy
            .create_software_tunnel()
            .context("Failed to create software tunnel")?;

        let stream = AdapterStream::connect(&mut adapter, rsd_port)
            .await
            .context("Failed to connect AdapterStream to RSD port")?;

        info!("Performing RSD Handshake...");
        let handshake = RsdHandshake::new(stream)
            .await
            .map_err(|e| anyhow::anyhow!("RSD Handshake failed: {:?}", e))?;

        let services: Vec<String> = handshake.services.keys().cloned().collect();
        info!(available_services = ?services, "RSD Handshake succeeded");

        if !services.iter().any(|s| s == DISPLAY_SERVICE) {
            warn!(
                service = DISPLAY_SERVICE,
                "Display service not found in RSD catalog. DDI (DeveloperDiskImage) may need mounting."
            );
        }

        Ok(Self {
            stream: handshake.into_stream(),
            services,
        })
    }

    pub fn has_display_service(&self) -> bool {
        self.services.iter().any(|s| s == DISPLAY_SERVICE)
    }
}
