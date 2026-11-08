// language: Rust, file: src/transport/rsd_tunnel.rs, target: macOS / Linux
use anyhow::{Context, Result};
use idevice::{
    core_device_proxy::CoreDeviceProxy, provider::IdeviceProvider, rsd::RsdHandshake,
    tcp::handle::AdapterHandle, IdeviceService,
};
use tracing::{info, warn};

pub const DISPLAY_SERVICE: &str = "com.apple.coredevice.displayservice";
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct RsdTunnel {
    pub adapter: AdapterHandle,
    pub handshake: RsdHandshake,
    pub services: Vec<String>,
}

impl RsdTunnel {
    /// Connect to device proxy, start software tunnel, and perform RSD handshake
    pub async fn connect(provider: Box<dyn IdeviceProvider>) -> Result<Self> {
        info!("Connecting to CoreDeviceProxy...");
        let proxy = tokio::time::timeout(CONNECT_TIMEOUT, CoreDeviceProxy::connect(&*provider))
            .await
            .context("Timed out connecting to CoreDeviceProxy")?
            .context("Failed to connect to CoreDeviceProxy")?;

        let rsd_port = proxy.tunnel_info().server_rsd_port;
        info!(rsd_port, "Starting software tunnel for RSD...");

        let adapter = proxy
            .create_software_tunnel()
            .context("Failed to create software tunnel")?;
        let mut adapter = adapter.to_async_handle();

        let stream = tokio::time::timeout(CONNECT_TIMEOUT, adapter.connect(rsd_port))
            .await
            .context("Timed out connecting to the RSD port")?
            .context("Failed to connect to the RSD port")?;

        info!("Performing RSD Handshake...");
        let handshake = tokio::time::timeout(CONNECT_TIMEOUT, RsdHandshake::new(stream))
            .await
            .context("Timed out during the RSD handshake")?
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
            adapter,
            handshake,
            services,
        })
    }

    pub fn has_display_service(&self) -> bool {
        self.services.iter().any(|s| s == DISPLAY_SERVICE)
    }

    pub fn sorted_services(&self) -> Vec<&str> {
        let mut services: Vec<_> = self.services.iter().map(String::as_str).collect();
        services.sort_unstable();
        services
    }
}
